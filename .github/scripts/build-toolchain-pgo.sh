#!/bin/bash
# Candidate macOS toolchain: the plain recipe of build-toolchain.sh with rustc PGO trained natively
# in the same build. Used only for Toolchains builds with pgo set to rustc; the release recipe is
# unchanged. Stages, evidence checks and tuning variables are described in docs/benchmarks.md.
# Usage: build-toolchain-pgo.sh <workdir> <output.tar.zst> <host-triple>
set -euo pipefail
work=$(mkdir -p "${1:?usage: build-toolchain-pgo.sh <workdir> <output.tar.zst> <host-triple>}" && cd "$1" && pwd)
out=$(cd "$(dirname "${2:?}")" && pwd)/$(basename "$2")
host=${3:?}
root=$(cd "$(dirname "$0")/../.." && pwd)
lock=$root/support/upstream.lock
pgo=$work/pgo
logs=$pgo/logs
evidence=$pgo/evidence.json
profile=$pgo/profiles/rustc-pgo.profdata
tools=$root/support/tools

get() {
    awk -F= -v k="$1" '$1 == k { print substr($0, index($0, "=") + 1); exit }' "$lock"
}

case $host in
    aarch64-apple-darwin) deployment=11.0 machine=arm64 ;;
    x86_64-apple-darwin) deployment=10.12 machine=x86_64 ;;
    *) echo "the rustc PGO candidate is built for the macOS targets only, not $host" >&2; exit 2 ;;
esac
[[ $(uname -s) == Darwin && $(uname -m) == "$machine" ]] || {
    echo "$host profiles are only valid on a native $machine macOS machine, this is $(uname -s) $(uname -m)" >&2
    exit 2
}

mkdir -p "$logs"
step() { printf '%s %s\n' "$(date -u +%Y-%m-%dT%H:%M:%SZ)" "$*" | tee -a "$pgo/progress.log"; }
trap 'step "FAILED at line $LINENO"' ERR

build_env=("MACOSX_DEPLOYMENT_TARGET=$deployment" "MACOSX_STD_DEPLOYMENT_TARGET=$deployment")
overrides=()
for file in "$root/support/bootstrap-common.toml" "$root/support/bootstrap-macos.toml"; do
    while IFS= read -r line || [[ -n $line ]]; do
        case $line in
            '' | '#'*) continue ;;
        esac
        overrides+=(--set "$line")
    done < "$file"
done

# The official release's llvm-tools carry the LLVM that rustc's profile runtime was built from.
release=$(get rust_release)
rustup toolchain install "$release" --profile minimal --component llvm-tools --no-self-update
official=$(rustup run "$release" rustc --print sysroot)
profdata=$official/lib/rustlib/$host/bin/llvm-profdata
[[ -x $profdata ]] || { echo "official $release has no llvm-profdata for $host" >&2; exit 2; }
profdata_version=$("$profdata" --version 2>&1)
[[ $profdata_version == *"$(get llvm_version)"* ]] || {
    echo "llvm-profdata of official $release is not LLVM $(get llvm_version): $profdata_version" >&2
    exit 2
}
step "profile tool $profdata ($(sed -n 2p <<< "$profdata_version" | sed 's/^ *//'))"

src=$work/rust
build=$work/build
git init -q "$src"
git -C "$src" fetch -q --depth 2 https://github.com/rust-lang/rust.git "$(get rust_commit)"
git -C "$src" update-ref --no-deref HEAD FETCH_HEAD
git -C "$src" read-tree --reset -u HEAD
git -C "$src" submodule update -q --init --depth 1 src/tools/cargo src/tools/rustc-perf
[[ $(git -C "$src/src/tools/cargo" rev-parse HEAD) == "$(get cargo_gitlink)" ]]
[[ $(git -C "$src/src/tools/rustc-perf" rev-parse HEAD) == "$(git -C "$src" rev-parse HEAD:src/tools/rustc-perf)" ]]
FRUST_RUST_SRC=$src "$root/support/apply-rustc-patches.sh"
FRUST_CARGO_SRC=$src/src/tools/cargo "$root/support/apply-patches.sh"
for patch in "$root"/support/patches/build/*.patch; do
    git -C "$src" apply --check "$patch"
    git -C "$src" apply "$patch"
    step "applied build patch $(basename "$patch")"
done

# Crates added by the build patch go first, so a time budget cuts upstream's stress crates.
upstream_crates=$(sed -n '/RUSTC_PGO_CRATES/,/^];/p' "$src/src/build_helper/src/lib.rs" | sed -n 's/^ *"\([^"]*\)",.*$/\1/p')
added_crates=$(sed -n 's/^+ *"\([^"]*\)",.*$/\1/p' "$root"/support/patches/build/*.patch)
[[ -n $added_crates && -n $upstream_crates ]] || { echo "cannot read the training crate list" >&2; exit 2; }
ordered_crates=$({ printf '%s\n' "$added_crates"; printf '%s\n' "$upstream_crates"; } | awk 'NF && !seen[$0]++' | paste -sd, -)
crates=${FRUST_PGO_TRAIN_CRATES:-$ordered_crates}
step "training crates: $crates"

awk -v dir="$build" '{ print } /^jobs = / { printf "build-dir = \"%s\"\n", dir }' "$root/support/bootstrap.toml" > "$work/bootstrap.toml"
jobs=$(getconf _NPROCESSORS_ONLN)
memory_gb=$(($(sysctl -n hw.memsize) / 1073741824))
tool_targets=(src/tools/clippy src/tools/rustfmt src/tools/rust-analyzer)

# -vv makes Cargo log every compiler command with its flags; -v alone does not.
xbuild() {
    local log=$1
    shift
    (cd "$src" && env RUST_BACKTRACE=1 "${build_env[@]}" python3 x.py build --config "$work/bootstrap.toml" \
        --stage 2 --jobs "${FRUST_PGO_BUILD_JOBS:-$jobs}" -vv "${overrides[@]}" "$@") > "$log" 2>&1 || {
        tail -n 200 "$log" >&2
        return 1
    }
}

fail=0
check() { python3 -I "$tools/pgo-rustc-evidence.py" "$evidence" "$@" || fail=1; }
finish_if_failed() {
    python3 -I "$tools/pgo-rustc-evidence.py" "$evidence" summary | tee "$pgo/summary.md"
    if [[ $fail != 0 ]]; then
        step "the PGO evidence failed; no toolchain is packed"
        exit 1
    fi
}
driver() {
    local found
    found=$(find "$build/$host/stage2/lib" -maxdepth 1 -name 'librustc_driver-*.dylib')
    [[ -n $found && $(printf '%s\n' "$found" | wc -l | tr -d ' ') == 1 ]] || {
        echo "expected one librustc_driver in $build/$host/stage2/lib, found: ${found:-none}" >&2
        return 1
    }
    printf '%s\n' "$found"
}

stage2=$build/$host/stage2
probe_runtime() {
    local file
    for file in "$stage2/bin/rustc" "$stage2/lib/"librustc_driver-*.dylib; do
        [[ -f $file ]] || continue
        codesign --verify --verbose=2 "$file" || true
        otool -L "$file" || true
    done
    vm_stat || true
    python3 -I - "$stage2/bin/rustc" "$host" "$stage2/lib:$build/$host/ci-llvm/lib" "$deployment" <<'PY'
import os
import subprocess
import sys

env = dict(os.environ, RUSTC_BOOTSTRAP="1", DYLD_LIBRARY_PATH=sys.argv[3],
           MACOSX_DEPLOYMENT_TARGET=sys.argv[4], MACOSX_STD_DEPLOYMENT_TARGET=sys.argv[4])
status = 0
for args in (["-vV"], ["--target", sys.argv[2], "-Zunstable-options", "--print=deployment-target"]):
    result = subprocess.run([sys.argv[1], *args], env=env, capture_output=True, timeout=30)
    print(f"rustc {args!r}: returncode={result.returncode}\nstdout={result.stdout!r}\nstderr={result.stderr!r}", flush=True)
    status |= int(result.returncode != 0 or not result.stdout)
sys.exit(status)
PY
}

generate=(--set "pgo.rustc.generate=\"$pgo/build-raw\"")
instrumented_jobs=$jobs
if ((memory_gb < 8 && jobs > 2)); then instrumented_jobs=2; fi
step "stage 1: instrumented stage-2 rustc using $instrumented_jobs jobs; log $logs/instrumented-build.log"
instrumented_status=0
FRUST_PGO_BUILD_JOBS=$instrumented_jobs xbuild "$logs/instrumented-build.log" "${generate[@]}" compiler || instrumented_status=$?
if [[ -x $stage2/bin/rustc ]]; then
    probe_runtime 2>&1 | tee "$logs/instrumented-launch.log" || fail=1
fi
if ((instrumented_status != 0 || fail != 0)); then exit 1; fi
check generate-flags "$logs/instrumented-build.log"
check driver "$(driver)" --expect instrumented
finish_if_failed
FRUST_PGO_BUILD_JOBS=$instrumented_jobs xbuild "$logs/instrumented-std.log" "${generate[@]}" library

stage0=$build/$host/stage0
step "building the rustc-perf collector with the stage-0 toolchain"
perf=$pgo/rustc-perf
cp -R "$src/src/tools/rustc-perf" "$perf"
rm -rf "$perf/.git"
(cd "$perf" && env RUSTC="$stage0/bin/rustc" RUSTC_BOOTSTRAP=1 "$stage0/bin/cargo" build -p collector) \
    > "$logs/collector-build.log" 2>&1 || { tail -n 100 "$logs/collector-build.log" >&2; exit 1; }

train_jobs=$((memory_gb / 3))
if ((train_jobs < 1)); then train_jobs=1; fi
if ((train_jobs > jobs)); then train_jobs=$jobs; fi
if ((train_jobs > 4)); then train_jobs=4; fi
step "stage 2: training with $train_jobs job(s) on $memory_gb GiB; unit logs in $pgo/train/logs"
python3 -I "$tools/pgo-rustc-train.py" \
    --rustc "$stage2/bin/rustc" --cargo "$stage0/bin/cargo" --collector "$perf/target/debug/collector" \
    --perf-dir "$perf" --llvm-profdata "$profdata" --host "$host" --crates "$crates" \
    --work "$pgo/train" --output "$profile" --evidence "$evidence" \
    --profiles "${FRUST_PGO_TRAIN_PROFILES:-Check,Debug,Opt}" --scenarios "${FRUST_PGO_TRAIN_SCENARIOS:-All}" \
    --jobs "$train_jobs" --budget-minutes "${FRUST_PGO_TRAIN_BUDGET_MINUTES:-80}" \
    --unit-timeout-minutes "${FRUST_PGO_UNIT_TIMEOUT_MINUTES:-25}" --min-crates "${FRUST_PGO_MIN_CRATES:-4}" \
    --exclude-cohorts "$root/qualification/cohorts.lock" 2>&1 | tee "$logs/train.log" || fail=1
finish_if_failed
rm -rf "$perf"

# Stages 0 and 1 are kept; the instrumented stage-2 outputs must not reach the package.
step "stage 3: removing the instrumented outputs, then the full recipe with the profile; log $logs/final-build.log"
rm -rf "$build/$host/stage2-rustc" "$build/$host/stage2"
xbuild "$logs/final-build.log" --set "pgo.rustc.use=\"$profile\"" library compiler rustdoc cargo "${tool_targets[@]}"
check use-flags "$logs/final-build.log" "$profile"
check warnings "$logs/final-build.log" --max-mismatch "${FRUST_PGO_MAX_MISMATCH_PERCENT:-5}" \
    --max-missing "${FRUST_PGO_MAX_MISSING_PERCENT:-50}"
check driver "$(driver)" --expect plain
finish_if_failed

{
    echo "target $host"
    echo "machine $(uname -m)"
    echo "macos $(sw_vers -productVersion)"
    echo "rust-commit $(get rust_commit)"
    echo "llvm-profdata $(sed -n 2p <<< "$profdata_version" | sed 's/^ *//')"
    echo "profile-sha256 $(shasum -a 256 "$profile" | cut -d' ' -f1)"
    echo "profile-bytes $(wc -c < "$profile" | tr -d ' ')"
    echo "training-crates $crates"
    echo "patches-sha256"
    shasum -a 256 "$root"/support/patches/rustc/*.patch "$root"/support/patches/*.patch "$root"/support/patches/build/*.patch \
        | sed "s|$root/||; s/^/  /"
} > "$pgo/MANIFEST"

cargo_bin=$build/$host/stage2-tools-bin/cargo
dest=$work/prefix/frust
mkdir -p "$work/prefix"
(cd "$root/crates" && cargo build --release --locked -p frust-rs --bin fcargo --target-dir "$work/fcargo")
FRUST_RUST_SRC=$src "$root/support/package-toolchain.sh" "$stage2" "$cargo_bin" \
    "$work/fcargo/release/fcargo" "$dest" "frust-$(get rust_commit | cut -c1-9)-ci" "$host"
rm -rf "$build" "$perf"
tar -cf - -C "$work/prefix" frust | zstd -T0 -3 -q > "$out"
echo "packed frust (rustc PGO candidate) for $host into $out"
