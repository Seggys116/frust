#!/usr/bin/env bash
# Build the host tools of the optimized Linux recipe from the pinned LLVM source:
# clang/clang++ (C parts of rustc: llvm-wrapper, jemalloc, compiler-rt), lld, llvm-ar/ranlib,
# llvm-profdata (PGO merge), llvm-bolt, merge-fdata and the BOLT instrumentation runtime.
#
# Usage: build-llvm-tools.sh LLVM_SOURCE PREFIX [JOBS]
#
# The result is cached: PREFIX/frust-llvm-tools.stamp records the LLVM commit and the configure
# flags, and a matching stamp skips the build. The build tree is PREFIX.build.
set -euo pipefail
if [[ $# -lt 2 || $# -gt 3 ]]; then
    echo "usage: $0 LLVM_SOURCE PREFIX [JOBS]" >&2
    exit 2
fi
src=$(realpath -- "$1")
prefix=$(realpath -m -- "$2")
jobs=${3:-$(nproc)}
build=$prefix.build
root=$(CDPATH= cd -- "$(dirname -- "$0")/../.." && pwd)
want=$(awk -F= '$1 == "llvm_gitlink" { print $2 }' "$root/support/upstream.lock")
have=$(git -C "$src" rev-parse HEAD)
if [[ $have != "$want" ]]; then
    echo "LLVM source is at $have, the pin is $want" >&2
    exit 1
fi
if [[ -n $(git -C "$src" status --porcelain --untracked-files=no) ]]; then
    echo "LLVM source has local modifications: $src" >&2
    exit 1
fi

# Host compilers for building the tools. Their version does not reach the Rust toolchain.
host_cc=${FRUST_TOOLS_HOST_CC:-clang}
host_cxx=${FRUST_TOOLS_HOST_CXX:-clang++}
flags=(
    -G Ninja
    -DCMAKE_BUILD_TYPE=Release
    -DCMAKE_C_COMPILER="$host_cc"
    -DCMAKE_CXX_COMPILER="$host_cxx"
    -DCMAKE_INSTALL_PREFIX="$prefix"
    -DLLVM_ENABLE_PROJECTS="clang;lld;bolt"
    -DLLVM_TARGETS_TO_BUILD=X86
    -DLLVM_ENABLE_ASSERTIONS=OFF
    -DLLVM_APPEND_VC_REV=OFF
    -DLLVM_INCLUDE_TESTS=OFF
    -DLLVM_INCLUDE_BENCHMARKS=OFF
    -DLLVM_INCLUDE_EXAMPLES=OFF
    -DLLVM_INCLUDE_DOCS=OFF
    -DLLVM_ENABLE_LIBXML2=OFF
    -DLLVM_ENABLE_Z3_SOLVER=OFF
    -DLLVM_PARALLEL_LINK_JOBS=4
    -DBOLT_ENABLE_RUNTIME=ON
    # Bootstrap compiles the compiler's C deps with -flto=thin and -fprofile-generate/-use, and
    # jemalloc's configure links test programs with them: clang needs an LTO-capable default
    # linker and its profile runtime (official CI uses LLVMgold with ld.bfd instead of lld).
    -DCLANG_DEFAULT_LINKER=lld
    -DLLVM_ENABLE_RUNTIMES=compiler-rt
    -DCOMPILER_RT_BUILD_PROFILE=ON
    -DCOMPILER_RT_BUILD_BUILTINS=OFF
    -DCOMPILER_RT_BUILD_SANITIZERS=OFF
    -DCOMPILER_RT_BUILD_XRAY=OFF
    -DCOMPILER_RT_BUILD_LIBFUZZER=OFF
    -DCOMPILER_RT_BUILD_MEMPROF=OFF
    -DCOMPILER_RT_BUILD_ORC=OFF
    -DCOMPILER_RT_BUILD_GWP_ASAN=OFF
    -DCOMPILER_RT_BUILD_CTX_PROFILE=OFF
    -DCOMPILER_RT_DEFAULT_TARGET_ONLY=ON
)
targets=(
    install-clang install-clang-resource-headers install-lld install-runtimes
    install-llvm-ar install-llvm-ranlib install-llvm-profdata
    install-llvm-bolt install-merge-fdata install-bolt_rt
)
stamp_text=$(printf '%s\n' "llvm $have" "host $("$host_cc" --version | sed -n 1p)" "${flags[@]}" "${targets[@]}")
if [[ -f $prefix/frust-llvm-tools.stamp && $(cat "$prefix/frust-llvm-tools.stamp") == "$stamp_text" ]]; then
    echo "cached $prefix"
    exit 0
fi

mkdir -p "$build"
cmake -S "$src/llvm" -B "$build" "${flags[@]}"
# BOLT's install targets share one install component that copies every BOLT tool, so all
# tools must exist before any install target runs.
ninja -C "$build" -j "$jobs" "${targets[@]#install-}"
ninja -C "$build" -j 1 "${targets[@]}"
for tool in clang clang++ ld.lld llvm-ar llvm-ranlib llvm-profdata llvm-bolt merge-fdata; do
    "$prefix/bin/$tool" --version > /dev/null
done
test -f "$prefix/lib/libbolt_rt_instr.a"
# A ThinLTO, PGO-instrumented C program must link with the default driver settings.
probe=$(mktemp -d "$build/probe.XXXXXX")
printf 'int main(void) { return 0; }\n' > "$probe/main.c"
"$prefix/bin/clang" -O2 -flto=thin -fprofile-generate="$probe" "$probe/main.c" -o "$probe/main"
"$probe/main"
rm -r "$probe"
printf '%s\n' "$stamp_text" > "$prefix/frust-llvm-tools.stamp"
"$prefix/bin/clang" --version
"$prefix/bin/llvm-bolt" --version
echo "built $prefix"
