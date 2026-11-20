#!/bin/bash
# Build a stage-2 toolchain from the pinned sources and pack it as a tar.zst.
# Usage: build-toolchain.sh <control|frust> <workdir> <output.tar.zst> [host-triple]
# control is the unpatched stage-2 with its stock Cargo. frust applies support/patches and is
# packaged with fcargo. Both use the same recipe for a given host, so the control stays a fair
# baseline: support/bootstrap.toml, the overrides of upstream's dist builds in
# support/bootstrap-common.toml and support/bootstrap-linux.toml or bootstrap-macos.toml, and the
# environment that linux_env or macos_env sets. The host defaults to the one in support/upstream.lock and
# must be the machine this runs on. Supported hosts: x86_64-unknown-linux-gnu, aarch64-apple-darwin
# and x86_64-apple-darwin.
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
variant=${1:?usage: build-toolchain.sh <control|frust> <workdir> <output.tar.zst> [host-triple]}
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
(cd "$src" && env RUST_BACKTRACE=1 "${build_env[@]}" python3 x.py build --config "$work/bootstrap.toml" --stage 2 \
    --jobs "$(getconf _NPROCESSORS_ONLN)" "${overrides[@]}" library compiler rustdoc cargo)

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
