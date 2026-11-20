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
#   LLVM_TOOLS   prefix produced by support/tools/build-llvm-tools.sh
#   PACKAGE_DEST toolchain directory to create (must not exist)
#   LAUNCHER     the fcargo-ui launcher binary to package
# Environment:
#   FRUST_OFFICIAL_TOOLCHAIN  official Rust used to build Cargo
#                             (default ~/.rustup/toolchains/<release>-x86_64-unknown-linux-gnu)
#   FRUST_BUILD_JOBS          parallel jobs (default: nproc)
#   FRUST_PROFILE_EXPORT      directory to receive the profiles and a MANIFEST (optional)
#   FRUST_TRAINING_SYSROOT    extracted libssl-dev tree (usr/include, usr/lib/<multiarch>) for
#                             the cargo-0.87.1 training crate when the host lacks OpenSSL headers
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

for tool in clang clang++ ld.lld llvm-ar llvm-ranlib llvm-profdata llvm-bolt merge-fdata; do
    "$tools/bin/$tool" --version > /dev/null
done
test -f "$tools/lib/libbolt_rt_instr.a"
host_strip=$(realpath -- "$(command -v strip)")
export FRUST_BOOTSTRAP_STRIP=$host_strip
[[ $("$FRUST_BOOTSTRAP_STRIP" --help) == *--keep-file-symbols* ]] || {
    echo "the Linux optimizer build requires strip --keep-file-symbols" >&2
    exit 2
}
bash "$root/support/verify-pins.sh" "$source_dir" "$work/pins.txt"
training=$(git -C "$source_dir" rev-parse HEAD:src/tools/rustc-perf)
[[ -f $source_dir/src/tools/rustc-perf/Cargo.toml &&
   $(git -C "$source_dir/src/tools/rustc-perf" rev-parse HEAD) == "$training" ]] || {
    echo "initialize the pinned rustc-perf submodule first" >&2
    exit 2
}

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
if ! printf '#include <openssl/opensslv.h>\n' | cc -E -x c - > /dev/null 2>&1; then
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

cd "$source_dir"
# Completed stages leave a marker; a rerun with the same work directory resumes after them.
if [[ -f $work/opt-dist.done ]]; then
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
        env -u RUSTFLAGS -u CARGO_TARGET_DIR \
            PATH="$official/bin:/usr/bin:/bin" \
            RUSTC="$official/bin/rustc" \
            CARGO_TARGET_DIR="$work/cargo-build" \
            CARGO_PROFILE_RELEASE_LTO=thin CARGO_PROFILE_RELEASE_CODEGEN_UNITS=1 \
            RUSTFLAGS="-Cprofile-use=$profile" \
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
