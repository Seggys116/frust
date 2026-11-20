#!/usr/bin/env bash
# Preflight for the LLVM build tools of support/optimize-linux-v2.sh: an install prefix from
# build-llvm-tools.sh, an extracted frust-build-tools bundle, or the bundle tarball itself
# (extracted to a fresh temporary directory, checked there and removed again).
#
# Usage: check-build-tools.sh TOOLS_DIR | TOOLS_TARBALL.tar.zst
#
# Checks, all against the given directory only (nothing from PATH except the shell utilities):
#   - every tool the recipe runs exists, starts and reports the pinned LLVM version;
#   - every shared library the tools need resolves (no "not found"), and none resolves to a
#     path inside the build tree recorded in the stamp;
#   - the stamp records the pinned LLVM commit;
#   - clang links a ThinLTO, PGO-instrumented C program with its default linker (lld) and its
#     own profile runtime, the program runs, and llvm-profdata merges its profile;
#   - clang++ compiles C++ and llvm-ar/llvm-ranlib archive the object;
#   - llvm-bolt instruments a program with its own runtime, the instrumented program writes
#     a profile, merge-fdata merges it, and llvm-bolt optimizes with it using the recipe's flags.
set -euo pipefail
if [[ $# -ne 1 ]]; then
    echo "usage: $0 TOOLS_DIR | TOOLS_TARBALL.tar.zst" >&2
    exit 2
fi
root=$(CDPATH= cd -- "$(dirname -- "$0")/../.." && pwd)
lock=$root/support/upstream.lock
want_commit=$(awk -F= '$1 == "llvm_gitlink" { print $2 }' "$lock")
want_version=$(awk -F= '$1 == "llvm_version" { print $2 }' "$lock")
[[ -n $want_commit && -n $want_version ]] || { echo "llvm pins missing from $lock" >&2; exit 2; }

scratch=$(mktemp -d "${TMPDIR:-/tmp}/frust-check-build-tools.XXXXXX")
trap 'rm -rf -- "$scratch"' EXIT
if [[ -f $1 ]]; then
    mkdir "$scratch/extract"
    tar --zstd -xf "$1" -C "$scratch/extract"
    tools=$(find "$scratch/extract" -mindepth 1 -maxdepth 1 -type d -print -quit)
    [[ -n $tools ]] || { echo "no top-level directory in $1" >&2; exit 1; }
    echo "extracted $1 to $tools"
else
    tools=$(realpath -- "$1")
fi
fail() { echo "check-build-tools: $*" >&2; exit 1; }

stamp=$tools/frust-llvm-tools.stamp
[[ -f $stamp ]] || fail "missing $stamp"
[[ $(sed -n 1p "$stamp") == "llvm $want_commit" ]] || fail "stamp is not for llvm $want_commit"
built_tree=$(sed -n 's/^-DCMAKE_INSTALL_PREFIX=//p' "$stamp")

bins=(clang clang++ ld.lld llvm-ar llvm-ranlib llvm-profdata llvm-bolt merge-fdata)
for tool in "${bins[@]}"; do
    [[ -x $tools/bin/$tool ]] || fail "missing $tools/bin/$tool"
    out=$("$tools/bin/$tool" --version 2>&1) || fail "$tool --version failed"
    [[ $out == *"$want_version"* ]] || fail "$tool does not report LLVM $want_version"
done
for file in lib/libbolt_rt_instr.a; do
    [[ -f $tools/$file ]] || fail "missing $tools/$file"
done
# Every distinct ELF the tools are made of (symlinks resolve to the same file).
mapfile -t elves < <(for tool in "${bins[@]}"; do realpath -- "$tools/bin/$tool"; done | sort -u)
for elf in "${elves[@]}"; do
    deps=$(ldd "$elf" 2>&1) || fail "ldd $elf failed: $deps"
    [[ $deps != *"not found"* ]] || fail "unresolved shared library for $elf: $deps"
    if [[ -n $built_tree && $deps == *"$built_tree"* ]]; then
        fail "$elf loads a library from the build tree $built_tree"
    fi
    echo "$(basename -- "$elf"): $(awk '$2 == "=>" { print $1 } $1 ~ /^\// { print $1 }' <<< "$deps" | tr '\n' ' ')"
done

cd "$scratch"
cat > main.c <<'C'
#include <stdio.h>
static int work(int n) { int s = 0; for (int i = 0; i < n; i++) s += (i % 3) ? i : -i; return s; }
int main(int argc, char **argv) { (void)argv; printf("%d\n", work(1000 * argc)); return 0; }
C
printf '#include <vector>\nint f() { std::vector<int> v{1, 2, 3}; return v[1]; }\n' > lib.cpp

# The recipe's C path: ThinLTO with profile instrumentation, default linker, own runtime.
"$tools/bin/clang" -O2 -flto=thin -fprofile-generate="$scratch/pgo" main.c -o pgo-gen
./pgo-gen > /dev/null
compgen -G "$scratch/pgo/*.profraw" > /dev/null || fail "instrumented program wrote no profile"
"$tools/bin/llvm-profdata" merge -o pgo.profdata "$scratch"/pgo/*.profraw
"$tools/bin/clang" -O2 -flto=thin -fprofile-use=pgo.profdata -Werror=profile-instr-unprofiled \
    main.c -o pgo-use
[[ $(./pgo-use) == "$(./pgo-gen)" ]] || fail "profile-use program output differs"

"$tools/bin/clang++" -O2 -c lib.cpp -o lib.o
"$tools/bin/llvm-ar" rc libprobe.a lib.o
"$tools/bin/llvm-ranlib" libprobe.a
"$tools/bin/llvm-ar" t libprobe.a | grep -qx lib.o || fail "llvm-ar archive lacks lib.o"

# The recipe's BOLT path: instrument, run, merge, optimize with the recipe's flags.
"$tools/bin/clang" -O2 -fno-pie -no-pie -Wl,--emit-relocs main.c -o bolt-in
"$tools/bin/llvm-bolt" bolt-in -instrument -instrumentation-file="$scratch/bolt-in.fdata" \
    -instrumentation-file-append-pid -o bolt-inst > bolt-instrument.log 2>&1 ||
    fail "llvm-bolt -instrument failed: $(tail -c 2000 bolt-instrument.log)"
./bolt-inst > /dev/null
compgen -G "$scratch/bolt-in.fdata.*" > /dev/null || fail "instrumented program wrote no fdata"
"$tools/bin/merge-fdata" "$scratch"/bolt-in.fdata.* > merged.fdata 2> merge-fdata.log ||
    fail "merge-fdata failed: $(tail -c 2000 merge-fdata.log)"
"$tools/bin/llvm-bolt" bolt-in -data merged.fdata -o bolt-out -reorder-blocks=ext-tsp \
    -reorder-functions=cdsort -split-functions -split-strategy=cdsplit -split-all-cold \
    -jump-tables=move -icf=all -update-debug-sections -dyno-stats > bolt-optimize.log 2>&1 ||
    fail "llvm-bolt optimize failed: $(tail -c 2000 bolt-optimize.log)"
[[ $(./bolt-out) == "$(./bolt-in)" ]] || fail "BOLT-optimized program output differs"
echo "build tools ok: $tools (LLVM $want_version, $want_commit)"
