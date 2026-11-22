#!/bin/bash
# Build a stage-2 toolchain from the pinned sources and pack it as a tar.zst.
# Usage: build-toolchain.sh <control|frust> <workdir> <output.tar.zst> [host-triple [cpu]]
# control is the unpatched stage-2 with its stock Cargo. frust applies support/patches and is
# packaged with fcargo. Both use the same recipe for a given host, so the control stays a fair
# baseline: support/bootstrap.toml, the overrides of upstream's dist builds in
# support/bootstrap-common.toml and support/bootstrap-linux.toml or bootstrap-macos.toml, and the
# environment that linux_env or macos_env sets. The host defaults to the one in support/upstream.lock and
# must be the machine this runs on. Supported hosts: x86_64-unknown-linux-gnu, aarch64-apple-darwin
# and x86_64-apple-darwin.
#
# The shipped Linux x86-64 toolchain (frust on x86_64-unknown-linux-gnu) is the exception: it is
# built by support/optimize-linux-v2.sh from stored PGO and BOLT profiles, with the clang, LLVM tools
# and profile bundles that support/fetch-build-assets.sh downloads (docs/build-assets.md). The
# optional cpu is baseline (the default, plain x86-64) or v3 (compiler and tools built for
# x86-64-v3); it only applies to that recipe. The shipped fRust toolchains for aarch64-apple-darwin
# and x86_64-apple-darwin are the other exception: the plain recipe with the compiler built by PGO from
# the stored profile bundle frust-pgo-profiles-<commit>-<target> of the same build assets, which
# .github/scripts/build-toolchain-pgo.sh stored checks against this build before it uses them.
# FRUST_RECIPE=plain forces the generic recipe below (no bundles needed, no PGO unless FRUST_PGO_PROFILES
# is set), FRUST_RECIPE=optimized insists on the Linux recipe, FRUST_RECIPE=pgo on the macOS one.
# FRUST_OFFICIAL_TOOLCHAIN names the official Rust that builds Cargo on Linux (default: the rustup
# toolchain of rust_release in support/upstream.lock). The control is always the plain recipe.
#
# Optional profile-guided optimization. When FRUST_PGO_PROFILES names a directory, the profiles in
# it are passed to bootstrap as pgo.<component>.use. The directory must hold a MANIFEST file with
# one "<component> <file>" pair per line (blank lines and lines starting with # are ignored),
# where <component> is rustc, rustdoc, cargo or clippy and <file> is the name of a merged
# .profdata file in the same directory. A llvm line is accepted and ignored, because LLVM is the
# prebuilt CI artifact. Profiles only fit the exact sources and settings they were collected
# from, and both variants use them when the variable is set. When the variable is unset the
# toolchain is built without PGO and the log says so.
set -euo pipefail
variant=${1:?usage: build-toolchain.sh <control|frust> <workdir> <output.tar.zst> [host-triple [cpu]]}
work=$(mkdir -p "${2:?}" && cd "$2" && pwd)
out=$(cd "$(dirname "${3:?}")" && pwd)/$(basename "$3")
root=$(cd "$(dirname "$0")/../.." && pwd)
lock=$root/support/upstream.lock
[[ $variant == control || $variant == frust ]] || { echo "unknown variant $variant" >&2; exit 2; }

get() {
    awk -F= -v k="$1" '$1 == k { print substr($0, index($0, "=") + 1); exit }' "$lock"
}

host=${4:-$(get host)}
case $host in
    x86_64-unknown-linux-gnu) platform=linux ;;
    aarch64-apple-darwin | x86_64-apple-darwin) platform=macos ;;
    *) echo "no upstream-matched recipe for host $host" >&2; exit 2 ;;
esac

cpu=${5:-}
case $cpu in
    '' | baseline | v3) ;;
    *) echo "unknown cpu $cpu, expected baseline or v3" >&2; exit 2 ;;
esac
recipe=${FRUST_RECIPE:-auto}
case $recipe in
    auto)
        if [[ $variant == frust && $host == x86_64-unknown-linux-gnu ]]; then
            recipe=optimized
        elif [[ $variant == frust && $platform == macos ]]; then
            recipe=pgo
        else
            recipe=plain
        fi
        ;;
    plain | optimized | pgo) ;;
    *) echo "unknown FRUST_RECIPE $recipe, expected auto, plain, optimized or pgo" >&2; exit 2 ;;
esac
if [[ $recipe == pgo ]]; then
    [[ $variant == frust && $platform == macos ]] || {
        echo "the pgo recipe builds the fRust toolchain for the macOS targets only" >&2
        exit 2
    }
    [[ -z $cpu || $cpu == baseline ]] || { echo "cpu $cpu is only for the Linux recipe" >&2; exit 2; }
fi
if [[ $recipe == optimized ]]; then
    [[ $variant == frust && $host == x86_64-unknown-linux-gnu ]] || {
        echo "the optimized recipe builds the fRust toolchain for x86_64-unknown-linux-gnu only" >&2
        exit 2
    }
    cpu=${cpu:-baseline}
elif [[ -n $cpu && $cpu != baseline ]]; then
    echo "cpu $cpu needs the optimized recipe" >&2
    exit 2
fi

# Environment for the bootstrap process only, as KEY=value words for env(1). Never empty.
build_env=()
# Bootstrap overrides as "--set" "key = value" pairs, collected from the recipe files. Never empty.
overrides=()

# Print the major version from the first line of "$1 --version" that has "$2 <number>.".
tool_major() {
    "$1" --version 2>&1 | sed -n "s/^.*$2 \([0-9][0-9]*\)\..*\$/\1/p" | sed -n 1p
}

# Upstream's dist-x86_64-linux image sets CC=clang CXX=clang++ and links with clang and lld. With
# clang as the C compiler bootstrap builds jemalloc and llvm-wrapper with -flto=thin, which the
# final link reads as LLVM bitcode, so lld must come from the same or a newer LLVM than clang.
# On ubuntu-24.04 the distribution packages clang, lld and llvm are all 18.1.x.
linux_env() {
    local tool clang_major lld_major ar libstdcxx
    for tool in clang clang++ lld ld.lld; do
        command -v "$tool" > /dev/null || { echo "$tool is not on PATH; install clang and lld" >&2; exit 2; }
    done
    clang_major=$(tool_major clang 'clang version' || true)
    lld_major=$(tool_major ld.lld 'LLD' || true)
    if [[ -n $clang_major && -n $lld_major ]]; then
        if (( lld_major < clang_major )); then
            echo "ld.lld $lld_major is older than clang $clang_major and cannot read its -flto=thin objects" >&2
            exit 2
        fi
    else
        echo "warning: cannot read the clang and lld versions, so their match is not checked" >&2
    fi
    # Archives of LTO objects need llvm-ar to get a symbol index that lld can use. Upstream sets
    # target.x86_64-unknown-linux-gnu.ar to its own llvm-ar for this.
    ar=$(command -v llvm-ar || command -v "llvm-ar-$clang_major" || true)
    [[ -n $ar ]] || { echo "llvm-ar is not on PATH; install llvm" >&2; exit 2; }
    # llvm.static-libstdcpp makes bootstrap ask the C++ compiler for libstdc++.a. A bare name back
    # means it found none, and the failure would only show at the end of a multi-hour build.
    libstdcxx=$(clang++ -print-file-name=libstdc++.a)
    [[ $libstdcxx == /* && -f $libstdcxx ]] || {
        echo "clang++ cannot find a static libstdc++ ($libstdcxx); install libstdc++-dev" >&2
        exit 2
    }
    echo "linux toolchain: clang $clang_major, lld ${lld_major:-unknown}, ar $ar, $libstdcxx"
    # LIBCURL_NO_PKG_CONFIG is set in upstream's image too: Cargo builds its own libcurl instead of
    # picking up the runner's.
    build_env=(CC=clang CXX=clang++ "AR=$ar" LIBCURL_NO_PKG_CONFIG=1)
}

# Upstream's macOS dist jobs build for the oldest macOS they support: 11.0 on aarch64, where
# nothing older exists, and 10.12 on x86_64. MACOSX_STD_DEPLOYMENT_TARGET is the variable
# bootstrap reads for the standard library, and the other one covers the host tools.
macos_env() {
    local deployment
    case $host in
        aarch64-apple-darwin) deployment=11.0 ;;
        *) deployment=10.12 ;;
    esac
    build_env=("MACOSX_DEPLOYMENT_TARGET=$deployment" "MACOSX_STD_DEPLOYMENT_TARGET=$deployment")
}

# Append each setting line of an override file to overrides. Comment lines and blank lines are
# skipped. The files hold one complete "section.key = value" setting per line.
read_overrides() {
    local line
    [[ -f $1 ]] || { echo "missing recipe file $1" >&2; exit 2; }
    while IFS= read -r line || [[ -n $line ]]; do
        case $line in
            '' | '#'*) continue ;;
        esac
        overrides+=(--set "$line")
    done < "$1"
}

# Append pgo.<component>.use for each profile that the MANIFEST in directory $1 lists.
add_pgo_profiles() {
    local dir manifest component file extra path used=0
    dir=$(cd "$1" 2> /dev/null && pwd) || { echo "FRUST_PGO_PROFILES is not a directory: $1" >&2; exit 2; }
    case $dir in
        *'"'* | *'\'*) echo "FRUST_PGO_PROFILES path must not contain quotes or backslashes: $dir" >&2; exit 2 ;;
    esac
    manifest=$dir/MANIFEST
    [[ -s $manifest ]] || { echo "no MANIFEST in $dir" >&2; exit 2; }
    while read -r component file extra || [[ -n $component ]]; do
        case $component in
            '' | '#'*) continue ;;
        esac
        [[ -n $file && -z $extra ]] || { echo "bad MANIFEST line: $component $file $extra" >&2; exit 2; }
        case $component in
            rustc | rustdoc | cargo | clippy) ;;
            llvm)
                echo "PGO: ignoring the llvm profile $file, LLVM is the prebuilt CI artifact"
                continue
                ;;
            *) echo "unknown MANIFEST component $component" >&2; exit 2 ;;
        esac
        case $file in
            '' | .* | *[!A-Za-z0-9._-]*) echo "bad profile file name in MANIFEST: $file" >&2; exit 2 ;;
        esac
        path=$dir/$file
        [[ -s $path ]] || { echo "profile $path is missing or empty" >&2; exit 2; }
        overrides+=(--set "pgo.$component.use = \"$path\"")
        used=$((used + 1))
    done < "$manifest"
    [[ $used -gt 0 ]] || { echo "MANIFEST in $dir lists no usable profile" >&2; exit 2; }
    echo "PGO: using $used profile(s) from $dir"
}

# The optimized Linux x86-64 recipe: support/optimize-linux-v2.sh in stored-profile mode. It trains
# nothing: the stage-2 tools are built with the stored PGO profiles, the rustc driver is
# BOLT-optimized with the stored data, and Cargo is rebuilt from the patched submodule. Its
# inputs, all pinned in support/upstream.lock and support/build-assets.lock, are
#   - the build tools bundle (clang, lld, llvm-ar, llvm-profdata, llvm-bolt, merge-fdata, the profile
#     and BOLT runtimes) of the pinned LLVM, which also compiles the C parts of rustc;
#   - the profile bundle of this Rust commit for the cpu (baseline or v3), each with a SHA256SUMS;
#   - official Rust, to build Cargo.
# The result is packed with the same layout as the plain recipe: a top directory named frust.
build_optimized() {
    local commit9 tools_asset profiles_asset dirs tools profiles libstdcxx official checkout package id
    local run_env=()
    commit9=$(get rust_commit | cut -c1-9)
    tools_asset=frust-build-tools-$(get llvm_version)-linux-x86_64.tar.zst
    id=frust-$commit9-ci
    if [[ $cpu == v3 ]]; then
        profiles_asset=frust-pgo-profiles-$commit9-x86-64-v3.tar.zst
        id=$id-v3
    else
        profiles_asset=frust-pgo-profiles-$commit9-x86-64.tar.zst
    fi

    # The downloads go through the work directory, which sits on the large disk of the runner.
    mkdir -p "$work/tmp"
    dirs=$(TMPDIR=$work/tmp bash "$root/support/fetch-build-assets.sh" "$work/assets" "$tools_asset" "$profiles_asset")
    tools=$(awk -v name="$tools_asset" '$1 == name { print $2 }' <<< "$dirs")
    profiles=$(awk -v name="$profiles_asset" '$1 == name { print $2 }' <<< "$dirs")
    [[ -d $tools && -d $profiles ]] || { echo "fetch-build-assets.sh did not unpack $tools_asset and $profiles_asset" >&2; exit 2; }
    # The bundled clang has to run on this machine and to find the system's static libstdc++. Check
    # both now, since a failure would otherwise only show once bootstrap links the compiler, hours
    # into the build.
    "$tools/bin/clang++" --version > /dev/null || { echo "the bundled clang++ does not run on this machine" >&2; exit 2; }
    libstdcxx=$("$tools/bin/clang++" -print-file-name=libstdc++.a)
    [[ $libstdcxx == /* && -f $libstdcxx ]] || {
        echo "the bundled clang++ cannot find a static libstdc++ ($libstdcxx); install libstdc++-dev" >&2
        exit 2
    }

    official=${FRUST_OFFICIAL_TOOLCHAIN:-$(rustup run "$(get rust_release)" rustc --print sysroot 2> /dev/null || true)}
    [[ -x $official/bin/rustc && -x $official/bin/cargo ]] || {
        echo "official Rust $(get rust_release) is not installed; the optimized recipe builds Cargo with it (set FRUST_OFFICIAL_TOOLCHAIN)" >&2
        exit 2
    }

    checkout=$work/checkout
    "$root/support/tools/prepare-rust-checkout.sh" "$checkout"
    # The shallow checkout holds the pinned merge commit only. Bootstrap resolves the prebuilt LLVM
    # from the first parent of that commit, as the generic recipe below notes, so keep one more.
    git -C "$checkout" fetch -q --depth 2 https://github.com/rust-lang/rust.git "$(get rust_commit)"

    (cd "$root/crates" && cargo build --release --locked -p frust-rs --bin fcargo --target-dir "$work/fcargo")

    package=$work/prefix/frust
    mkdir -p "$work/prefix"
    run_env=("FRUST_OFFICIAL_TOOLCHAIN=$official" "FRUST_STORED_PROFILES=$profiles")
    if [[ $cpu == v3 ]]; then
        run_env+=(FRUST_COMPILER_TARGET_CPU=x86-64-v3)
    fi
    echo "optimized recipe for $host: cpu $cpu, tools $tools, profiles $profiles"
    env "${run_env[@]}" "$root/support/optimize-linux-v2.sh" \
        "$checkout" "$work/opt" "$tools" "$package" "$work/fcargo/release/fcargo" "$id"

    # The logs stay in $work/opt for the workflow to upload; the build trees are what fills the disk.
    rm -rf "$work/opt/build" "$work/opt/cargo-build" "$work/opt/package-host"
    tar -cf - -C "$work/prefix" frust | zstd -T0 -3 -q > "$out"
    echo "packed frust ($cpu) for $host into $out"
}

# The macOS recipe: rustc PGO from the stored profile bundle of this Rust commit and target. Nothing is
# trained. The bundle is a build asset like the Linux profiles (docs/build-assets.md); without it in
# support/build-assets.lock the build stops here, since shipping a macOS toolchain without its profile
# would silently give up what the recipe is for. FRUST_RECIPE=plain builds without PGO.
build_pgo() {
    local asset dirs profiles
    asset=frust-pgo-profiles-$(get rust_commit | cut -c1-9)-$host.tar.zst
    if ! grep -q "^$asset " "${FRUST_BUILD_ASSETS_LOCK:-$root/support/build-assets.lock}"; then
        echo "support/build-assets.lock has no $asset. Generate and publish the macOS profiles first" \
            "(the macOS profiles workflow, docs/build-assets.md), or set FRUST_RECIPE=plain to build without PGO" >&2
        exit 2
    fi
    mkdir -p "$work/tmp"
    dirs=$(TMPDIR=$work/tmp bash "$root/support/fetch-build-assets.sh" "$work/assets" "$asset")
    profiles=$(awk -v name="$asset" '$1 == name { print $2 }' <<< "$dirs")
    [[ -d $profiles ]] || { echo "fetch-build-assets.sh did not unpack $asset" >&2; exit 2; }
    echo "pgo recipe for $host: profiles $profiles"
    "$root/.github/scripts/build-toolchain-pgo.sh" stored "$work" "$out" "$host" "$profiles"
}

if [[ $recipe == optimized ]]; then
    build_optimized
    exit 0
fi
if [[ $recipe == pgo ]]; then
    build_pgo
    exit 0
fi

read_overrides "$root/support/bootstrap-common.toml"
read_overrides "$root/support/bootstrap-$platform.toml"
if [[ $platform == linux ]]; then linux_env; else macos_env; fi
if [[ -n ${FRUST_PGO_PROFILES:-} ]]; then
    add_pgo_profiles "$FRUST_PGO_PROFILES"
else
    echo "PGO: FRUST_PGO_PROFILES is not set, building without profile-guided optimization"
fi
echo "recipe for $host:"
for ((i = 1; i < ${#overrides[@]}; i += 2)); do
    echo "  ${overrides[i]}"
done
printf '  %s\n' "${build_env[@]}"

src=$work/rust
build=$work/build
git init -q "$src"
# Bootstrap resolves the prebuilt LLVM from the first parent of the pinned merge commit.
git -C "$src" fetch -q --depth 2 https://github.com/rust-lang/rust.git "$(get rust_commit)"
git -C "$src" -c advice.detachedHead=false checkout -q --detach FETCH_HEAD
# Cargo is patched before bootstrap runs, so its submodule is populated here.
git -C "$src" submodule update -q --init --depth 1 src/tools/cargo
[[ $(git -C "$src/src/tools/cargo" rev-parse HEAD) == "$(get cargo_gitlink)" ]]

if [[ $variant == frust ]]; then
    FRUST_RUST_SRC=$src "$root/support/apply-rustc-patches.sh"
    FRUST_CARGO_SRC=$src/src/tools/cargo "$root/support/apply-patches.sh"
fi

# build-dir goes right after jobs, inside [build]. awk keeps this working with BSD tools.
awk -v dir="$build" '{ print } /^jobs = / { printf "build-dir = \"%s\"\n", dir }' "$root/support/bootstrap.toml" > "$work/bootstrap.toml"
# The shipped fRust toolchain carries clippy and rustfmt. They link the compiler's own crates
# (rustc_private), so they can only be built here, against this exact compiler. Bootstrap builds
# cargo-clippy, clippy-driver, rustfmt and cargo-fmt for these paths and puts them in
# <stage2>/bin, with an rpath of @loader_path/../lib (macOS) or $ORIGIN/../lib so they find the
# compiler's shared libraries. src/tools/rust-analyzer builds rust-analyzer and, beside it,
# rust-analyzer-proc-macro-srv, which bootstrap installs in <stage2>/libexec, the directory where
# rust-analyzer looks for it. Both link the compiler's driver library, so the official binaries
# (which name the official driver) cannot be used. The unpatched control is only the baseline
# compiler and Cargo.
tool_targets=()
if [[ $variant == frust ]]; then
    tool_targets=(src/tools/clippy src/tools/rustfmt src/tools/rust-analyzer)
fi
# ${tool_targets[@]+...}: an empty array is an unbound variable for the bash 3.2 of macOS.
(cd "$src" && env RUST_BACKTRACE=1 "${build_env[@]}" python3 x.py build --config "$work/bootstrap.toml" --stage 2 \
    --jobs "$(getconf _NPROCESSORS_ONLN)" "${overrides[@]}" library compiler rustdoc cargo \
    ${tool_targets[@]+"${tool_targets[@]}"})

stage2=$build/$host/stage2
cargo_bin=$build/$host/stage2-tools-bin/cargo
dest=$work/prefix/$variant
mkdir -p "$work/prefix"
if [[ $variant == control ]]; then
    cp -a "$stage2" "$dest"
    install -m 755 "$cargo_bin" "$dest/bin/cargo"
else
    (cd "$root/crates" && cargo build --release --locked -p frust-rs --bin fcargo --target-dir "$work/fcargo")
    FRUST_RUST_SRC=$src "$root/support/package-toolchain.sh" "$stage2" "$cargo_bin" \
        "$work/fcargo/release/fcargo" "$dest" "frust-$(get rust_commit | cut -c1-9)-ci" "$host"
fi
rm -rf "$build"
tar -cf - -C "$work/prefix" "$variant" | zstd -T0 -3 -q > "$out"
echo "packed $variant for $host into $out"
