#!/usr/bin/env bash
# Optimized Linux x86-64 fRust toolchain, revision 2.
#
# Differences from support/optimize-linux.sh, each matching the official dist-x86_64-linux build
# (src/ci/docker/host-x86_64/dist-x86_64-linux, src/ci/run.sh) unless noted:
#   1. The C and C++ parts of the compiler (llvm-wrapper, jemalloc, compiler-rt) are compiled by
#      clang from the pinned LLVM; bootstrap then adds -flto=thin for the compiler's C deps, and
#      llvm-ar/llvm-ranlib archive the bitcode.
#   2. libstdc++ is linked statically into rustc_driver (llvm.static-libstdcpp).
#   3. llvm-profdata, llvm-bolt and merge-fdata come from the pinned LLVM (build-llvm-tools.sh).
#   4. The rustc PGO training set adds application-style rustc-perf crates
#      (support/patches/build/, not upstream; the measured cohorts stay out).
#   5. build.optimized-compiler-builtins and rust.lld are set explicitly.
# Cargo is then rebuilt from the patched submodule like the uc4 package: official Rust as the
# compiler, ThinLTO, one codegen unit, the merged rustc/cargo profile, static native deps.
#
# Usage: optimize-linux-v2.sh RUST_SOURCE WORK LLVM_TOOLS PACKAGE_DEST LAUNCHER [PACKAGE_ID]
#   RUST_SOURCE  isolated checkout from support/tools/prepare-rust-checkout.sh
#   WORK         fresh work directory (build tree, profiles, logs, progress.log)
#   LLVM_TOOLS   prefix produced by support/tools/build-llvm-tools.sh, or an extracted
#                frust-build-tools bundle (support/tools/package-build-tools.sh); both are
#                checked by support/tools/check-build-tools.sh before anything is built
#   PACKAGE_DEST toolchain directory to create (must not exist)
#   LAUNCHER     the fcargo-ui launcher binary to package
# Environment:
#   FRUST_OFFICIAL_TOOLCHAIN  official Rust used to build Cargo
#                             (default ~/.rustup/toolchains/<release>-x86_64-unknown-linux-gnu)
#   FRUST_BUILD_JOBS          parallel jobs (default: nproc)
#   FRUST_PROFILE_EXPORT      directory to receive the profiles and a MANIFEST (optional)
#   FRUST_TRAINING_SYSROOT    extracted libssl-dev tree (usr/include, usr/lib/<multiarch>) for
#                             the cargo-0.87.1 training crate when the host lacks OpenSSL headers
#   FRUST_STORED_PROFILES     directory holding rustc-pgo, rustdoc-pgo, clippy-pgo and rustc-bolt
#                             .profdata with a SHA256SUMS (a FRUST_PROFILE_EXPORT of an earlier
#                             run). Training is skipped: the stage-2 tools are built with these PGO
#                             profiles and the rustc driver is BOLT-optimized with the stored data,
#                             exactly as opt-dist does after its training stages. The MANIFEST
#                             must name the same rust and llvm pins and a target-cpu equal to
#                             FRUST_COMPILER_TARGET_CPU (x86-64 when unset); otherwise it is refused.
#   FRUST_COMPILER_TARGET_CPU target CPU (for example x86-64-v3) for the compiler and tools only:
#                             crates built into stage2-rustc/ and stage2-tools/ and Cargo. The
#                             standard library and compiler_builtins keep the baseline target.
set -euo pipefail

if [[ $# -lt 5 || $# -gt 6 ]]; then
    echo "usage: $0 RUST_SOURCE WORK LLVM_TOOLS PACKAGE_DEST LAUNCHER [PACKAGE_ID]" >&2
    exit 2
fi
root=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)
source_dir=$(realpath -- "$1")
work=$(realpath -m -- "$2")
tools=$(realpath -- "$3")
package=$(realpath -m -- "$4")
launcher=$(realpath -- "$5")
package_id=${6:-$(basename -- "$package")}
host=x86_64-unknown-linux-gnu
jobs=${FRUST_BUILD_JOBS:-$(nproc)}
release=$(awk -F= '$1 == "rust_release" { print $2 }' "$root/support/upstream.lock")
official=${FRUST_OFFICIAL_TOOLCHAIN:-$HOME/.rustup/toolchains/$release-$host}
# Resolve relative paths now; the build later runs inside the source checkout.
[[ -z ${FRUST_PROFILE_EXPORT:-} ]] || FRUST_PROFILE_EXPORT=$(realpath -m -- "$FRUST_PROFILE_EXPORT")
[[ -z ${FRUST_TRAINING_SYSROOT:-} ]] || FRUST_TRAINING_SYSROOT=$(realpath -- "$FRUST_TRAINING_SYSROOT")
[[ -z ${FRUST_STORED_PROFILES:-} ]] || FRUST_STORED_PROFILES=$(realpath -- "$FRUST_STORED_PROFILES")
target_cpu=${FRUST_COMPILER_TARGET_CPU:-}
[[ $target_cpu =~ ^[A-Za-z0-9_.-]*$ ]] || { echo "invalid FRUST_COMPILER_TARGET_CPU" >&2; exit 2; }

[[ $(uname -s) == Linux && $(uname -m) == x86_64 ]] || {
    echo "this recipe requires Linux x86-64" >&2
    exit 2
}
[[ ! -e $package ]] || { echo "package destination exists: $package" >&2; exit 1; }
[[ -x $launcher ]] || { echo "launcher is not executable: $launcher" >&2; exit 2; }
[[ -x $official/bin/rustc && -x $official/bin/cargo ]] || {
    echo "official toolchain missing: $official" >&2
    exit 2
}
mkdir -p "$work/tmp" "$work/bootstrap-tools"
progress=$work/progress.log
step() { printf '%s %s\n' "$(date -Is)" "$*" | tee -a "$progress"; }
trap 'step "FAILED at line $LINENO"' ERR
step "start source=$source_dir tools=$tools package=$package jobs=$jobs"

bash "$root/support/tools/check-build-tools.sh" "$tools" > "$work/build-tools-check.txt" 2>&1 || {
    cat "$work/build-tools-check.txt" >&2
    exit 2
}
step "build tools checked ($work/build-tools-check.txt)"
host_strip=$(realpath -- "$(command -v strip)")
export FRUST_BOOTSTRAP_STRIP=$host_strip
[[ $("$FRUST_BOOTSTRAP_STRIP" --help) == *--keep-file-symbols* ]] || {
    echo "the Linux optimizer build requires strip --keep-file-symbols" >&2
    exit 2
}
bash "$root/support/verify-pins.sh" "$source_dir" "$work/pins.txt"
training=$(git -C "$source_dir" rev-parse HEAD:src/tools/rustc-perf)
if [[ -n ${FRUST_STORED_PROFILES:-} ]]; then
    (cd "$FRUST_STORED_PROFILES" && sha256sum --quiet -c SHA256SUMS)
    for name in rustc-pgo rustdoc-pgo clippy-pgo rustc-bolt; do
        test -s "$FRUST_STORED_PROFILES/$name.profdata"
    done
    # PGO and BOLT profiles only fit the code they were trained on: the same sources and the
    # same compiler target CPU (target features change the IR before instrumentation).
    stored_manifest=$FRUST_STORED_PROFILES/MANIFEST
    [[ -f $stored_manifest ]] || { echo "stored profiles lack a MANIFEST" >&2; exit 2; }
    stored_cpu=$(awk '$1 == "target-cpu" { print $2 }' "$stored_manifest")
    [[ -n $stored_cpu ]] || { echo "stored MANIFEST has no target-cpu line" >&2; exit 2; }
    if [[ $stored_cpu != "${target_cpu:-x86-64}" ]]; then
        echo "stored profiles were trained with target-cpu $stored_cpu;" \
            "FRUST_COMPILER_TARGET_CPU requests ${target_cpu:-x86-64}" >&2
        exit 2
    fi
    for pin in rust llvm; do
        if [[ $(awk -v p="$pin" '$1 == "pin" && $2 == p { print $3 }' "$stored_manifest") != \
              $(awk -v p="$pin" '$1 == p { print $2 }' "$work/pins.txt") ]]; then
            echo "stored profiles were trained on a different $pin commit" >&2
            exit 2
        fi
    done
    step "stored profiles $FRUST_STORED_PROFILES (target-cpu $stored_cpu)"
elif [[ ! -f $source_dir/src/tools/rustc-perf/Cargo.toml ||
        $(git -C "$source_dir/src/tools/rustc-perf" rev-parse HEAD) != "$training" ]]; then
    echo "initialize the pinned rustc-perf submodule first" >&2
    exit 2
fi

# Rustc and Cargo series: only patches tracked by git; untracked files there are other work in
# progress. support/patches/build/ belongs to this recipe alone and is applied as it stands.
# Later patches of a series edit the same files as earlier ones, so a partly or fully applied
# series cannot be recognised patch by patch. A stamp in the repository's git directory records
# the exact series applied; a rerun with the same series skips it.
apply_series() {
    local repo=$1 pattern=$2 name=$3 patch digest stamp
    local patches=()
    mapfile -t patches < <(if [[ $pattern == /* ]]; then
                               compgen -G "$pattern" | sort || true
                           else
                               git -C "$root" ls-files -- ":(glob)$pattern" | sort
                           fi)
    for i in "${!patches[@]}"; do
        [[ ${patches[i]} == /* ]] || patches[i]=$root/${patches[i]}
    done
    [[ ${#patches[@]} -gt 0 ]] || return 0
    digest=$(sha256sum "${patches[@]}" | sed "s|$root/||")
    stamp=$(git -C "$repo" rev-parse --absolute-git-dir)/frust-series-$name
    if [[ -f $stamp && $(cat "$stamp") == "$digest" ]]; then
        step "series $name already applied"
        return 0
    fi
    for patch in "${patches[@]}"; do
        if ! git -C "$repo" apply --check "$patch" > /dev/null 2>&1; then
            echo "patch does not apply: $patch" >&2
            return 1
        fi
        git -C "$repo" apply "$patch"
        step "applied $(basename -- "$patch")"
    done
    printf '%s\n' "$digest" > "$stamp"
}
apply_series "$source_dir" 'support/patches/rustc/*.patch' rustc
apply_series "$source_dir/src/tools/cargo" 'support/patches/*.patch' cargo
apply_series "$source_dir" "$root/support/patches/build/*.patch" build
{
    git -C "$source_dir" diff --stat
    git -C "$source_dir/src/tools/cargo" diff --stat
    git -C "$source_dir/src/tools/cargo" status --short
} > "$work/source-changes.txt"

cat > "$work/bootstrap-tools/strip" <<'BASH'
#!/usr/bin/env bash
exec "$FRUST_BOOTSTRAP_STRIP" --keep-file-symbols "$@"
BASH
chmod +x "$work/bootstrap-tools/strip"
# The pinned LLVM tools follow the strip wrapper, so clang, ld.lld and llvm-ar resolve to them.
export PATH="$work/bootstrap-tools:$tools/bin:$PATH"
config=$work/bootstrap-optimized.toml
python3 - "$root/support/bootstrap.toml" "$config" "$work/build" "$tools" "$jobs" <<'PY'
import json
from pathlib import Path
import re
import sys

source, destination, build, tools, jobs = sys.argv[1:]
config = Path(source).read_text()
config, count = re.subn(r"(?m)^jobs = \d+$", f"jobs = {int(jobs)}", config)
assert count == 1, "support/bootstrap.toml must set build.jobs once"
config = config.replace(
    "[build]\n", f"[build]\nbuild-dir = {json.dumps(build)}\n"
    'metrics = true\nsubmodules = false\nallocator = "jemalloc"\nprofiler = true\n'
    'cargo-native-static = true\noptimized-compiler-builtins = true\n'
).replace(
    "[rust]\n", "[rust]\ncodegen-units = 1\ncodegen-units-std = 1\n"
    "debuginfo-level-std = 1\nbootstrap-override-lld = true\nlld = true\n"
).replace(
    "[llvm]\n", "[llvm]\nstatic-libstdcpp = true\n"
)
bin = Path(tools) / "bin"
config += (
    "\n[target.x86_64-unknown-linux-gnu]\n"
    f"linker = {json.dumps(str(bin / 'clang'))}\n"
    f"cc = {json.dumps(str(bin / 'clang'))}\n"
    f"cxx = {json.dumps(str(bin / 'clang++'))}\n"
    f"ar = {json.dumps(str(bin / 'llvm-ar'))}\n"
    f"ranlib = {json.dumps(str(bin / 'llvm-ranlib'))}\n"
)
Path(destination).write_text(config)
PY
export RUST_BOOTSTRAP_CONFIG=$config
export TMPDIR=$work/tmp
# Training runs the patched Cargo as plain `cargo`; keep every fRust default out of it anyway.
export FRUST_UNIT_CACHE=0 FRUST_CENTRAL_TARGET=off FRUST_GC=0
if [[ -n ${FRUST_TRAINING_SYSROOT:-} ]]; then
    sdk=$(realpath -- "$FRUST_TRAINING_SYSROOT")
    export CPATH="$sdk/usr/include:$sdk/usr/include/x86_64-linux-gnu${CPATH:+:$CPATH}"
    export OPENSSL_INCLUDE_DIR=$sdk/usr/include OPENSSL_LIB_DIR=$sdk/usr/lib/x86_64-linux-gnu
    step "training SDK headers $sdk"
fi
if [[ -z ${FRUST_STORED_PROFILES:-} ]] &&
    ! printf '#include <openssl/opensslv.h>\n' | cc -E -x c - > /dev/null 2>&1; then
    echo "OpenSSL headers are required by the cargo-0.87.1 training crate;" \
        "set FRUST_TRAINING_SYSROOT" >&2
    exit 2
fi
bootstrap_python=$work/bootstrap-python
cat > "$bootstrap_python" <<'BASH'
#!/usr/bin/env bash
set -euo pipefail
args=("$@")
# The profiling workloads also need proc_macro and test in the training sysroot.
if [[ ${args[1]:-} == build ]]; then
    for i in "${!args[@]}"; do
        if [[ ${args[i]} == library/std ]]; then
            args[i]=library
        fi
    done
fi
exec python3 "${args[@]}"
BASH
chmod +x "$bootstrap_python"

# Compiler-only target CPU. rust.rustflags and RUSTFLAGS_NOT_BOOTSTRAP also reach the standard
# library, so the flag is added by a rustc wrapper instead: bootstrap hands an inherited
# RUSTC_WRAPPER to its shim as RUSTC_WRAPPER_REAL, and every rustc that Cargo starts sees the
# CARGO_TARGET_DIR bootstrap chose for the mode. stage2-rustc holds the compiler crates and
# stage2-tools the rustdoc and clippy builds; stage*-std and stage1-* keep the baseline CPU.
if [[ -n $target_cpu ]]; then
    cat > "$work/bootstrap-tools/compiler-cpu-rustc" <<'BASH'
#!/usr/bin/env bash
case ${CARGO_TARGET_DIR:-} in
    */stage2-rustc | */stage2-tools) exec "$@" "-Ctarget-cpu=$FRUST_COMPILER_TARGET_CPU" ;;
esac
exec "$@"
BASH
    chmod +x "$work/bootstrap-tools/compiler-cpu-rustc"
    export FRUST_COMPILER_TARGET_CPU=$target_cpu
    export RUSTC_WRAPPER=$work/bootstrap-tools/compiler-cpu-rustc
    step "compiler target CPU $target_cpu (wrapper $RUSTC_WRAPPER)"
fi
bolt_flags=(-reorder-blocks=ext-tsp -reorder-functions=cdsort -split-functions
    -split-strategy=cdsplit -split-all-cold -jump-tables=move -icf=all
    -update-debug-sections -dyno-stats)

cd "$source_dir"
# Completed stages leave a marker; a rerun with the same work directory resumes after them.
if [[ -n ${FRUST_STORED_PROFILES:-} ]]; then
    # The steps opt-dist runs after training (Stage 1 "Build PGO optimized rustc", Stage 3
    # "Optimize rustc with BOLT", Stage 5 dist), fed with the stored profiles.
    mkdir -p "$work/profiles"
    for name in rustc-pgo rustdoc-pgo clippy-pgo rustc-bolt; do
        cmp -s "$FRUST_STORED_PROFILES/$name.profdata" "$work/profiles/$name.profdata" ||
            cp "$FRUST_STORED_PROFILES/$name.profdata" "$work/profiles/$name.profdata"
    done
    pgo_use=(--set "pgo.rustc.use=\"$work/profiles/rustc-pgo.profdata\""
        --set "pgo.cargo.use=\"$work/profiles/rustc-pgo.profdata\""
        --set "pgo.rustdoc.use=\"$work/profiles/rustdoc-pgo.profdata\""
        --set "pgo.clippy.use=\"$work/profiles/clippy-pgo.profdata\"")
    if [[ ! -f $work/stored-build.done ]]; then
        step "build PGO-optimized stage 2 from stored profiles; log $work/stored-build.log"
        RUST_BACKTRACE=full python3 x.py build --target "$host" --host "$host" --stage 2 \
            library rustdoc cargo clippy "${pgo_use[@]}" --enable-bolt-settings \
            > "$work/stored-build.log" 2>&1
        touch "$work/stored-build.done"
    fi
    if [[ ! -f $work/stored-bolt.done ]]; then
        rustc_lib=$(find "$work/build/$host/stage2/lib" -maxdepth 1 \
            -name 'librustc_driver-*.so' -print -quit)
        # Keep the PGO-only input: it is what the stored BOLT profile must match.
        cp "$rustc_lib" "$work/rustc-driver-prebolt.so"
        step "BOLT $rustc_lib with the stored profile; log $work/stored-bolt.log"
        # Like opt-dist, write through the existing path so that its hard links follow.
        "$tools/bin/llvm-bolt" "$work/rustc-driver-prebolt.so" \
            -data "$work/profiles/rustc-bolt.profdata" -o "$rustc_lib" "${bolt_flags[@]}" \
            > "$work/stored-bolt.log" 2>&1
        touch "$work/stored-bolt.done"
    fi
    if [[ ! -f $work/opt-dist.done ]]; then
        step "dist (keeping the BOLT-optimized stage 2); log $work/stored-dist.log"
        RUST_BACKTRACE=full python3 x.py dist rustc rust-std cargo rust-src "${pgo_use[@]}" \
            --keep-stage 0 --keep-stage 1 \
            --reproducible-artifact "$work/profiles/rustc-bolt.profdata" \
            > "$work/stored-dist.log" 2>&1
        touch "$work/opt-dist.done"
    fi
elif [[ -f $work/opt-dist.done ]]; then
    step "opt-dist already finished"
else
    step "build opt-dist"
    python3 x.py build --stage 1 opt-dist > "$work/opt-dist-build.log" 2>&1
    step "opt-dist local (PGO training, PGO build, BOLT, dist); log $work/opt-dist.log"
    "$work/build/$host/stage1-tools-bin/opt-dist" local \
        --target-triple "$host" --checkout-dir "$source_dir" \
        --llvm-dir "$tools" --artifact-dir "$work/profiles" --build-dir "$work/build" \
        --python "$bootstrap_python" --build-llvm=false --use-bolt \
        -- python3 x.py dist rustc rust-std cargo rust-src > "$work/opt-dist.log" 2>&1
    touch "$work/opt-dist.done"
    step "opt-dist finished"
fi
profile=$work/profiles/rustc-pgo.profdata
test -s "$profile"
test -s "$work/profiles/rustc-bolt.profdata"

# Cargo for the fRust half, mirroring the uc4 package build.
if [[ -f $work/cargo.done ]]; then
    step "Cargo already built"
else
    step "build Cargo (official rustc, ThinLTO, 1 CGU, profile-use, all-static); log $work/cargo-build.log"
    (
        cd "$source_dir/src/tools/cargo"
        env -u RUSTFLAGS -u CARGO_TARGET_DIR -u RUSTC_WRAPPER \
            PATH="$official/bin:/usr/bin:/bin" \
            RUSTC="$official/bin/rustc" \
            CARGO_TARGET_DIR="$work/cargo-build" \
            CARGO_PROFILE_RELEASE_LTO=thin CARGO_PROFILE_RELEASE_CODEGEN_UNITS=1 \
            RUSTFLAGS="-Cprofile-use=$profile${target_cpu:+ -Ctarget-cpu=$target_cpu}" \
            "$official/bin/cargo" build --release --locked -j "$jobs" --features all-static
    ) > "$work/cargo-build.log" 2>&1
    touch "$work/cargo.done"
fi
cargo_bin=$work/cargo-build/release/cargo

package_toolchain() {
    step "package $package"
    # The stage-2 sysroot has no rustdoc; the PGO-optimized one is stage2-tools-bin's
    # rustdoc-tool-binary (the uc4 package used the same file). package-toolchain.sh copies the
    # rustc-dev rlibs from every build directory beside the sysroot, and stage2-rustc still holds
    # the PGO-instrumented build's outputs under other hashes. Stage hard-linked copies holding
    # only the newest build directory per crate, which is the final (profile-use) build.
    local host_dir=$work/package-host/$host
    local staged=$host_dir/stage2 sub crate newest
    rm -rf "$work/package-host"
    mkdir -p "$host_dir"
    cp -al "$work/build/$host/stage2" "$staged"
    install -m 755 "$work/build/$host/stage2-tools-bin/rustdoc-tool-binary" "$staged/bin/rustdoc"
    # Target units (rlibs) and host units (proc-macro dylibs) have separate build roots.
    for sub in "stage2-rustc/$host/release/build" "stage2-rustc/release/build"; do
        [[ -d $work/build/$host/$sub ]] || continue
        mkdir -p "$host_dir/$sub"
        for crate in "$work/build/$host/$sub"/*/; do
            crate=${crate%/}
            compgen -G "$crate/*/out" > /dev/null || continue
            newest=$(ls -td "$crate"/*/out | sed -n 1p)
            newest=${newest%/out}
            mkdir -p "$host_dir/$sub/$(basename "$crate")"
            cp -al "$newest" "$host_dir/$sub/$(basename "$crate")/"
        done
    done
    FRUST_RUST_SRC=$source_dir bash "$root/support/package-toolchain.sh" \
        "$staged" "$cargo_bin" "$launcher" "$package" "$package_id" > "$work/package.log" 2>&1
}
package_toolchain
driver=$(find "$package/lib" -maxdepth 1 -name 'librustc_driver-*.so' -print -quit)
{
    env -i PATH=/usr/bin:/bin "$package/bin/rustc" -vV
    env -i PATH=/usr/bin:/bin "$package/bin/cargo" -vV
    readelf -p .comment "$driver" "$package/bin/rustc"
    readelf -d "$driver" | grep NEEDED
    readelf -n "$driver" | grep -A2 bolt_info
    sha256sum "$package/bin/rustc" "$driver" "$package"/lib/libLLVM* "$package/bin/cargo" \
        "$work"/profiles/*.profdata
} > "$work/package-provenance.txt" 2>&1
if readelf -d "$driver" | grep -q 'libstdc++'; then
    step "ERROR rustc_driver still needs a shared libstdc++"
    exit 1
fi

if [[ -n ${FRUST_PROFILE_EXPORT:-} ]]; then
    export_dir=$(realpath -m -- "$FRUST_PROFILE_EXPORT")
    mkdir -p "$export_dir"
    cp "$work"/profiles/*.profdata "$export_dir/"
    (cd "$export_dir" && sha256sum -- *.profdata > SHA256SUMS)
    {
        echo "package $package_id"
        echo "recipe support/optimize-linux-v2.sh"
        echo "target-cpu ${target_cpu:-x86-64}"
        if [[ -n $target_cpu ]]; then
            echo "compiler-flags -Ctarget-cpu=$target_cpu for stage2-rustc and stage2-tools" \
                "(RUSTC_WRAPPER), PGO-instrumented and profile-use builds alike, and Cargo;" \
                "standard library baseline"
        else
            echo "compiler-flags none beyond the bootstrap config (baseline x86-64)"
        fi
        echo "frust-commit $(git -C "$root" rev-parse HEAD)"
        echo "frust-recipe-sha256 $(sha256sum "$root/support/optimize-linux-v2.sh" | cut -d' ' -f1)"
        echo "profiles-from ${FRUST_STORED_PROFILES:-training in this run (opt-dist local)}"
        sed 's/^/pin /' "$work/pins.txt"
        echo "tools $("$tools/bin/clang" --version | sed -n 1p)"
        echo "tools $("$tools/bin/llvm-profdata" --version 2>&1 | sed -n 2p | sed 's/^ *//')"
        echo "tools $("$tools/bin/llvm-bolt" --version 2>&1 | grep -m1 -i 'version')"
        sed 's/^/tools-stamp /' "$tools/frust-llvm-tools.stamp"
        echo "training rustc-perf $training"
        echo "training rustc/cargo/rustdoc/clippy crates (Check,Debug,Opt x All; Doc; Clippy):"
        sed -n '/RUSTC_PGO_CRATES/,/^];/p' "$source_dir/src/build_helper/src/lib.rs" | sed 's/^/  /'
        echo "bolt rustc_driver training: same crates, rustc_benchmarks"
        echo "bolt flags: -reorder-blocks=ext-tsp -reorder-functions=cdsort -split-functions"
        echo "  -split-strategy=cdsplit -split-all-cold -jump-tables=move -icf=all"
        echo "  -update-debug-sections -dyno-stats"
        echo "bootstrap config:"
        sed 's/^/  /' "$config"
        echo "patches applied (sha256):"
        cat "$(git -C "$source_dir" rev-parse --absolute-git-dir)"/frust-series-* \
            "$(git -C "$source_dir/src/tools/cargo" rev-parse --absolute-git-dir)"/frust-series-* \
            | sed 's/^/  /'
    } > "$export_dir/MANIFEST"
    step "profiles exported to $export_dir"
fi
step "done $package"
