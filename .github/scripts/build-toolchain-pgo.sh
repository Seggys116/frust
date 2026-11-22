#!/bin/bash
# macOS fRust toolchain with rustc PGO: the plain recipe of build-toolchain.sh, with the compiler built
# by profile-guided optimization. Stages, evidence checks and tuning variables are described in
# docs/benchmarks.md and docs/build-assets.md.
#
# Usage:
#   build-toolchain-pgo.sh stored    <workdir> <output.tar.zst> <host-triple> <profile-dir>
#   build-toolchain-pgo.sh generate  <workdir> <output.tar.zst> <host-triple>
#   build-toolchain-pgo.sh candidate <workdir> <output.tar.zst> <host-triple>
#
# stored     The shipped macOS recipe. No training: the compiler is built with the profile bundle that
#            support/fetch-build-assets.sh unpacked into <profile-dir> (docs/build-assets.md). The bundle is
#            refused unless it was made for this Rust commit, target, machine, LLVM and set of build
#            settings (support/tools/pgo-rustc-bundle.py check), and the build is refused when too many
#            functions no longer match the profile. build-toolchain.sh calls this mode.
# generate   Profile generation, run by the macOS profiles workflow and never by a release. Builds the
#            instrumented stage-2 rustc, trains it natively on the rustc-perf crates, merges the profile,
#            then builds the profile-use compiler once to prove the profile fits (set
#            FRUST_PGO_VERIFY=0 to stop after training). The output is the build asset
#            frust-pgo-profiles-<commit>-<target>.tar.zst, written only when every gate passed.
# candidate  Training and the profile-use build in one job, packed as a toolchain: the experimental
#            macos-*-pgo benchmark platforms. It needs no stored profile.
set -euo pipefail
mode=${1:-}
case $mode in
    stored | generate | candidate) shift ;;
    *)
        echo "usage: build-toolchain-pgo.sh stored|generate|candidate <workdir> <output.tar.zst> <host-triple> [profile-dir]" >&2
        exit 2
        ;;
esac
work=$(mkdir -p "${1:?missing <workdir>}" && cd "$1" && pwd)
out=$(cd "$(dirname "${2:?missing <output.tar.zst>}")" && pwd)/$(basename "$2")
host=${3:?missing <host-triple>}
stored_dir=
if [[ $mode == stored ]]; then
    stored_dir=$(cd "${4:?stored mode needs <profile-dir>}" && pwd)
fi
root=$(cd "$(dirname "$0")/../.." && pwd)
lock=$root/support/upstream.lock
pgo=$work/pgo
logs=$pgo/logs
evidence=$pgo/evidence.json
profile=$pgo/profiles/rustc-pgo.profdata
cargo_profile=$pgo/profiles/cargo-pgo.profdata
tools=$root/support/tools
# Cargo can get a profile of its own: trained in generate and candidate builds when FRUST_PGO_CARGO=1
# (off by default until a macOS run has proven it), and used by a stored build when the bundle holds one.
with_cargo=0
if [[ ${FRUST_PGO_CARGO:-0} == 1 ]]; then with_cargo=1; fi

get() {
    awk -F= -v k="$1" '$1 == k { print substr($0, index($0, "=") + 1); exit }' "$lock"
}

case $host in
    aarch64-apple-darwin) deployment=11.0 machine=arm64 ;;
    x86_64-apple-darwin) deployment=10.12 machine=x86_64 ;;
    *) echo "the macOS PGO recipe is built for the macOS targets only, not $host" >&2; exit 2 ;;
esac
[[ $(uname -s) == Darwin && $(uname -m) == "$machine" ]] || {
    echo "$host profiles are only valid on a native $machine macOS machine, this is $(uname -s) $(uname -m)" >&2
    exit 2
}

mkdir -p "$logs"
step() { printf '%s %s\n' "$(date -u +%Y-%m-%dT%H:%M:%SZ)" "$*" | tee -a "$pgo/progress.log"; }
trap 'step "FAILED at line $LINENO"' ERR
step "mode $mode for $host"

# Apple Clang and the pinned Rust LLVM can write incompatible profile records (rust-lang/rust#163640).
target_env=${host//-/_}
cflags=CFLAGS_$target_env
cxxflags=CXXFLAGS_$target_env
no_c_pgo='-fno-profile-generate -fno-profile-use'
build_env=("MACOSX_DEPLOYMENT_TARGET=$deployment" "MACOSX_STD_DEPLOYMENT_TARGET=$deployment"
    "$cflags=${!cflags-} $no_c_pgo" "$cxxflags=${!cxxflags-} $no_c_pgo")
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

fail=0
check() { python3 -I "$tools/pgo-rustc-evidence.py" "$evidence" "$@" || fail=1; }
summary_title='Native rustc PGO candidate'
if [[ $mode == stored ]]; then summary_title='Stored rustc PGO profile'; fi
if [[ $mode == generate ]]; then summary_title='macOS PGO profile generation'; fi
finish_if_failed() {
    python3 -I "$tools/pgo-rustc-evidence.py" "$evidence" summary --title "$summary_title" | tee "$pgo/summary.md"
    if [[ $fail != 0 ]]; then
        step "the PGO evidence failed; nothing is packed"
        exit 1
    fi
}

# A stored profile is judged before anything is fetched or built, so a stale bundle fails in a minute
# and not after hours of building.
if [[ $mode == stored ]]; then
    mkdir -p "$pgo/profiles"
    python3 -I "$tools/pgo-rustc-bundle.py" check "$root" "$stored_dir" --host "$host" \
        --rust-commit "$(get rust_commit)" --llvm-version "$(get llvm_version)" --llvm-profdata "$profdata" \
        --evidence "$evidence" || fail=1
    finish_if_failed
    profile=$stored_dir/rustc-pgo.profdata
    with_cargo=0
    if [[ -f $stored_dir/cargo-pgo.profdata ]]; then
        with_cargo=1
        cargo_profile=$stored_dir/cargo-pgo.profdata
    fi
fi

src=$work/rust
build=$work/build
git init -q "$src"
git -C "$src" fetch -q --depth 2 https://github.com/rust-lang/rust.git "$(get rust_commit)"
git -C "$src" update-ref --no-deref HEAD FETCH_HEAD
git -C "$src" read-tree --reset -u HEAD
submodules=(src/tools/cargo)
if [[ $mode != stored ]]; then submodules+=(src/tools/rustc-perf); fi
git -C "$src" submodule update -q --init --depth 1 "${submodules[@]}"
[[ $(git -C "$src/src/tools/cargo" rev-parse HEAD) == "$(get cargo_gitlink)" ]]
if [[ $mode != stored ]]; then
    [[ $(git -C "$src/src/tools/rustc-perf" rev-parse HEAD) == "$(git -C "$src" rev-parse HEAD:src/tools/rustc-perf)" ]]
fi
FRUST_RUST_SRC=$src "$root/support/apply-rustc-patches.sh"
FRUST_CARGO_SRC=$src/src/tools/cargo "$root/support/apply-patches.sh"
crates=
if [[ $mode != stored ]]; then
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
fi

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
cargo_bin=$build/$host/stage2-tools-bin/cargo
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

# Evidence of a failed first launch: backtrace, profile sections, crash report and an environment
# differential. It only records; the launch gate below still decides, and a failure here is ignored.
launch_diagnostics() {
    step "the instrumented rustc failed its first launch; collecting native diagnostics in $pgo/diagnostics"
    python3 -I "$tools/pgo-rustc-launch-diagnostics.py" launch --rustc "$stage2/bin/rustc" --host "$host" \
        --lib-path "$stage2/lib:$build/$host/ci-llvm/lib" --deployment "$deployment" --profile-dir "$pgo/build-raw" \
        --build-log "$logs/instrumented-build.log" --out "$pgo/diagnostics" > "$logs/instrumented-launch-diagnostics.log" 2>&1 || true
    if [[ -f $pgo/diagnostics/summary.md && ! -f $pgo/summary.md ]]; then cp "$pgo/diagnostics/summary.md" "$pgo/summary.md"; fi
}

# Stages 1 and 2: the instrumented compiler, then native training. Leaves the merged profile in $profile.
train() {
    local generate instrumented_targets instrumented_jobs instrumented_status stage0 perf train_jobs
    generate=(--set "pgo.rustc.generate=\"$pgo/build-raw\"")
    instrumented_targets=(compiler)
    if ((with_cargo)); then
        generate+=(--set "pgo.cargo.generate=\"$pgo/cargo-build-raw\"")
        instrumented_targets+=(cargo)
    fi
    instrumented_jobs=$jobs
    if ((memory_gb < 8 && jobs > 2)); then instrumented_jobs=2; fi
    step "stage 1: instrumented stage-2 rustc using $instrumented_jobs jobs; log $logs/instrumented-build.log"
    instrumented_status=0
    FRUST_PGO_BUILD_JOBS=$instrumented_jobs xbuild "$logs/instrumented-build.log" "${generate[@]}" "${instrumented_targets[@]}" || instrumented_status=$?
    if [[ -x $stage2/bin/rustc ]]; then
        probe_runtime 2>&1 | tee "$logs/instrumented-launch.log" || { fail=1; launch_diagnostics; }
    fi
    if ((with_cargo)) && [[ -x $cargo_bin ]]; then
        # The instrumented Cargo must start before it is trained, like the compiler.
        { "$cargo_bin" --version && otool -L "$cargo_bin"; } 2>&1 | tee "$logs/instrumented-cargo-launch.log" || fail=1
    fi
    if ((instrumented_status != 0 || fail != 0)); then exit 1; fi
    check generate-flags "$logs/instrumented-build.log"
    check driver "$(driver)" --expect instrumented
    if ((with_cargo)); then check driver "$cargo_bin" --expect instrumented --key instrumented_cargo; fi
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
    if ((with_cargo)); then
        step "stage 2b: training the instrumented Cargo with $train_jobs job(s); unit logs in $pgo/train-cargo/logs"
        python3 -I "$tools/pgo-cargo-train.py" \
            --cargo "$cargo_bin" --fetch-cargo "$stage0/bin/cargo" --rustc "$stage0/bin/rustc" \
            --perf-dir "$perf" --llvm-profdata "$profdata" \
            --crates "${FRUST_PGO_CARGO_CRATES:-serde-1.0.219,clap_derive-4.5.32,html5ever-0.31.0,hyper-1.6.0,eza-0.21.2}" \
            --work "$pgo/train-cargo" --output "$cargo_profile" --evidence "$evidence" --jobs "$train_jobs" \
            --noops "${FRUST_PGO_CARGO_NOOPS:-4}" --budget-minutes "${FRUST_PGO_CARGO_BUDGET_MINUTES:-30}" \
            --unit-timeout-minutes "${FRUST_PGO_UNIT_TIMEOUT_MINUTES:-25}" \
            --min-crates "${FRUST_PGO_CARGO_MIN_CRATES:-3}" \
            --exclude-cohorts "$root/qualification/cohorts.lock" 2>&1 | tee "$logs/train-cargo.log" || fail=1
        finish_if_failed
    fi
    rm -rf "$perf"
}

# The profile-use build, judged by the evidence checks. $1 is the log, the rest names what to build.
# A build that follows training starts from the stages 0 and 1 it left; the instrumented stage-2
# outputs must not reach the package.
use_build() {
    local log cargo_set also
    log=$1
    cargo_set=()
    also=()
    shift
    rm -rf "$build/$host/stage2-rustc" "$build/$host/stage2"
    if ((with_cargo)); then
        # An instrumented Cargo of the training stage must not survive either.
        rm -rf "$build/$host/stage2-tools" "$build/$host/stage2-tools-bin"
        cargo_set=(--set "pgo.cargo.use=\"$cargo_profile\"")
        also=(--also "$cargo_profile")
    fi
    xbuild "$log" --set "pgo.rustc.use=\"$profile\"" ${cargo_set[@]+"${cargo_set[@]}"} "$@"
    check use-flags "$log" "$profile" ${also[@]+"${also[@]}"}
    check warnings "$log" --max-mismatch "${FRUST_PGO_MAX_MISMATCH_PERCENT:-5}" \
        --max-missing "${FRUST_PGO_MAX_MISSING_PERCENT:-50}"
    check driver "$(driver)" --expect plain
    if ((with_cargo)); then check driver "$cargo_bin" --expect plain --key final_cargo; fi
    finish_if_failed
}

if [[ $mode != stored ]]; then train; fi

if [[ $mode == generate ]]; then
    if [[ ${FRUST_PGO_VERIFY:-1} != 0 ]]; then
        step "verifying the profile with a profile-use compiler build; log $logs/verify-build.log"
        if ((with_cargo)); then
            use_build "$logs/verify-build.log" compiler cargo
        else
            use_build "$logs/verify-build.log" compiler
        fi
    else
        step "FRUST_PGO_VERIFY=0: the profile was not used in a build"
    fi
    bundle=$pgo/bundle
    components=(--component "rustc=$profile")
    if ((with_cargo)); then components+=(--component "cargo=$cargo_profile"); fi
    python3 -I "$tools/pgo-rustc-bundle.py" export "$root" "$bundle" --host "$host" \
        --rust-commit "$(get rust_commit)" --llvm-version "$(get llvm_version)" --llvm-profdata "$profdata" \
        --evidence "$evidence" --rustc-perf "$(git -C "$src" rev-parse HEAD:src/tools/rustc-perf)" \
        "${components[@]}"
    (cd "$bundle" && COPYFILE_DISABLE=1 tar -cf - *) | zstd -19 -T0 -q > "$out"
    cp "$bundle/MANIFEST" "$pgo/MANIFEST"
    step "packed the profile bundle for $host into $out ($(wc -c < "$out" | tr -d ' ') bytes)"
    rm -rf "$build"
    exit 0
fi

# Stages 0 and 1 of a candidate build are kept; a stored build starts from nothing.
step "full recipe with the profile; log $logs/final-build.log"
use_build "$logs/final-build.log" library compiler rustdoc cargo "${tool_targets[@]}"

if [[ $mode == stored ]]; then
    {
        cat "$stored_dir/MANIFEST"
        echo "built-on-macos $(sw_vers -productVersion)"
        echo "built-profile-sha256 $(shasum -a 256 "$profile" | cut -d' ' -f1)"
    } > "$pgo/MANIFEST"
else
    {
        echo "target $host"
        echo "machine $(uname -m)"
        echo "macos $(sw_vers -productVersion)"
        echo "rust-commit $(get rust_commit)"
        echo "llvm-profdata $(sed -n 2p <<< "$profdata_version" | sed 's/^ *//')"
        echo "profile-sha256 $(shasum -a 256 "$profile" | cut -d' ' -f1)"
        echo "profile-bytes $(wc -c < "$profile" | tr -d ' ')"
        if ((with_cargo)); then echo "cargo-profile-sha256 $(shasum -a 256 "$cargo_profile" | cut -d' ' -f1)"; fi
        echo "training-crates $crates"
        echo "patches-sha256"
        shasum -a 256 "$root"/support/patches/rustc/*.patch "$root"/support/patches/*.patch "$root"/support/patches/build/*.patch \
            | sed "s|$root/||; s/^/  /"
    } > "$pgo/MANIFEST"
fi

cargo_bin=$build/$host/stage2-tools-bin/cargo
dest=$work/prefix/frust
mkdir -p "$work/prefix"
(cd "$root/crates" && cargo build --release --locked -p frust-rs --bin fcargo --target-dir "$work/fcargo")
FRUST_RUST_SRC=$src "$root/support/package-toolchain.sh" "$stage2" "$cargo_bin" \
    "$work/fcargo/release/fcargo" "$dest" "frust-$(get rust_commit | cut -c1-9)-ci" "$host"
rm -rf "$build"
tar -cf - -C "$work/prefix" frust | zstd -T0 -3 -q > "$out"
echo "packed frust (rustc PGO, $mode) for $host into $out"
