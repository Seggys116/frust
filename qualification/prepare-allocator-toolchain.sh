#!/usr/bin/env bash
# Build a Linux comparison package whose frustc alone loads jemalloc.
set -euo pipefail

if [[ $# != 4 ]]; then
    echo "usage: $0 BASE_TOOLCHAIN OUTPUT_TOOLCHAIN LIBJEMALLOC PATCHELF" >&2
    exit 2
fi

base=$(realpath -- "$1")
output=$(realpath -m -- "$2")
allocator=$(realpath -- "$3")
patchelf=$(realpath -- "$4")
if [[ ! -x "$base/bin/frustc" || ! -d "$base/lib" || -L "$base/lib" ||
      ! -x "$patchelf" || -e "$output" || -L "$output" ]]; then
    echo "expected a packaged compiler, patchelf, and a new output directory" >&2
    exit 2
fi
soname=$("$patchelf" --print-soname "$allocator")
if [[ "$soname" != libjemalloc.so.2 || -e "$base/lib/$soname" || -L "$base/lib/$soname" ]]; then
    echo "expected a jemalloc library absent from the base package" >&2
    exit 2
fi

cp -a --reflink=auto -- "$base" "$output"
install -m 755 -- "$allocator" "$output/lib/$soname"
# rustc and frustc may be hard links in the base package.
staged="$output/bin/.frustc-allocator"
cp --preserve=mode -- "$base/bin/frustc" "$staged"
"$patchelf" --add-needed "$soname" "$staged"
mv -T -- "$staged" "$output/bin/frustc"
cmp -- "$base/bin/rustc" "$output/bin/rustc"
if ! env -u LD_PRELOAD -u LD_LIBRARY_PATH -u LD_AUDIT LC_ALL=C \
        LD_DEBUG=bindings LD_BIND_NOW=1 "$output/bin/frustc" -Vv 2>&1 |
    awk -v package="$output/" '
        /binding file .*libLLVM.* to .*libjemalloc\.so\.2 .*normal symbol / &&
        index($0, " to " package) && index($0, "`malloc" sprintf("%c", 39)) {bound = 1}
        !/^[[:space:]]*[0-9]+:/ && NF {print}
        END {exit !bound}
    '; then
    echo "LLVM malloc did not bind to the packaged allocator" >&2
    exit 1
fi
