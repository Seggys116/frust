#!/usr/bin/env bash
# Gather a BOLT profile for a stage-2 rustc driver the way opt-dist's Stage 3 does
# (with_bolt_instrumented + gather_bolt_profiles in src/tools/opt-dist), for a build that was
# made by optimize-linux-v2.sh with FRUST_STORED_PROFILES and so skipped opt-dist's training.
#
# Usage: bolt-train-driver.sh WORK LLVM_TOOLS RUSTC_PERF OUTPUT
#   WORK        optimize-linux-v2.sh work directory; WORK/rustc-driver-prebolt.so is the
#               PGO-optimized driver before BOLT and WORK/build/<host>/stage2 its sysroot
#   LLVM_TOOLS  prefix produced by build-llvm-tools.sh (llvm-bolt, merge-fdata, bolt runtime)
#   RUSTC_PERF  rustc-perf tree with a built target/debug/collector (opt-dist leaves one in its
#               artifact directory, WORK/profiles/rustc-perf, after a training run)
#   OUTPUT      merged .fdata to write
# Environment: FRUST_TRAINING_SYSROOT as for optimize-linux-v2.sh (cargo-0.87.1 needs OpenSSL).
# The stage-2 driver is replaced by the instrumented one for the run and restored afterwards.
set -euo pipefail
if [[ $# -ne 4 ]]; then
    echo "usage: $0 WORK LLVM_TOOLS RUSTC_PERF OUTPUT" >&2
    exit 2
fi
root=$(CDPATH= cd -- "$(dirname -- "$0")/../.." && pwd)
work=$(realpath -- "$1")
tools=$(realpath -- "$2")
perf=$(realpath -- "$3")
output=$(realpath -m -- "$4")
host=x86_64-unknown-linux-gnu
sysroot=$work/build/$host/stage2
prebolt=$work/rustc-driver-prebolt.so
collector=$perf/target/debug/collector
test -s "$prebolt"
test -x "$collector"
driver=$(find "$sysroot/lib" -maxdepth 1 -name 'librustc_driver-*.so' -print -quit)
test -n "$driver"

# The crate list opt-dist trains on (src/build_helper RUSTC_PGO_CRATES with the build patch).
source_dir=$(sed -n 's/^.*start source=\([^ ]*\) .*$/\1/p' "$work/progress.log" | sed -n '$p')
crates=$(sed -n '/RUSTC_PGO_CRATES/,/^];/p' "$source_dir/src/build_helper/src/lib.rs" |
    sed -n 's/^ *"\([^"]*\)",.*$/\1/p' | paste -sd, -)
[[ -n $crates ]]

export FRUST_UNIT_CACHE=0 FRUST_CENTRAL_TARGET=off FRUST_GC=0
export TMPDIR=$work/tmp
export PATH="$work/bootstrap-tools:$tools/bin:$PATH"
unset RUSTC_WRAPPER
if [[ -n ${FRUST_TRAINING_SYSROOT:-} ]]; then
    sdk=$(realpath -- "$FRUST_TRAINING_SYSROOT")
    export CPATH="$sdk/usr/include:$sdk/usr/include/x86_64-linux-gnu${CPATH:+:$CPATH}"
    export OPENSSL_INCLUDE_DIR=$sdk/usr/include OPENSSL_LIB_DIR=$sdk/usr/lib/x86_64-linux-gnu
fi

profiles=$work/bolt-train
[[ ! -e $profiles ]] || { echo "$profiles exists from an earlier run" >&2; exit 1; }
mkdir -p "$profiles"
backup=$work/rustc-driver-installed.so
cp "$driver" "$backup"
restore() { cp "$backup" "$driver"; }
trap restore EXIT

"$tools/bin/llvm-bolt" -instrument "$prebolt" \
    "--instrumentation-file=$profiles/prof.fdata" --instrumentation-file-append-pid \
    -o "$work/rustc-driver-instrumented.so" > "$work/bolt-instrument.log" 2>&1
cp "$work/rustc-driver-instrumented.so" "$driver"

(
    cd "$perf"
    "$collector" profile_local eprintln "$sysroot/bin/rustc" --id Test \
        --cargo "$work/build/$host/stage2-tools-bin/cargo" \
        --profiles Check,Debug,Opt --scenarios All --exact-match "$crates" \
        --frontend-threads 1 --jobs 4
) > "$work/bolt-train.log" 2>&1

restore
trap - EXIT
"$tools/bin/merge-fdata" "$profiles"/prof.fdata* > "$output"
echo "merged $(ls "$profiles" | wc -l) profiles into $output ($(stat -c %s "$output") bytes)"
