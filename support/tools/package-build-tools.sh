#!/usr/bin/env bash
# Package the LLVM build tools of the stored-profile recipe (support/optimize-linux-v2.sh) as a
# relocatable tarball for CI: clang/clang++ with their resource headers and the compiler-rt
# profile runtime, lld (ld.lld), llvm-ar/llvm-ranlib, llvm-profdata, llvm-bolt with its
# instrumentation runtime, and merge-fdata. Nothing else from the install prefix is included.
#
# Usage: package-build-tools.sh PREFIX OUT.tar.zst [SYSROOT_PACKAGES_TXT]
#   PREFIX  install prefix of build-llvm-tools.sh, built with FRUST_TOOLS_SYSROOT for portability
#   OUT     tarball to create (must not exist); its one top-level directory is OUT's base name
#           without .tar.zst, and can be extracted anywhere and passed to the recipe as LLVM_TOOLS
#
# Every symlink is relative, binaries are stripped, and the archive is reproducible (sorted,
# fixed owner and mtime). The tools are checked for host paths and glibc symbol versions.
set -euo pipefail
if [[ $# -lt 2 || $# -gt 3 ]]; then
    echo "usage: $0 PREFIX OUT.tar.zst [SYSROOT_PACKAGES_TXT]" >&2
    exit 2
fi
prefix=$(realpath -- "$1")
out=$(realpath -m -- "$2")
packages=${3:-}
[[ $out == *.tar.zst ]] || { echo "OUT must end in .tar.zst" >&2; exit 2; }
[[ ! -e $out ]] || { echo "exists: $out" >&2; exit 1; }
name=$(basename -- "$out" .tar.zst)
max_glibc=${FRUST_TOOLS_MAX_GLIBC:-2.39}
stage=$(mktemp -d "$(dirname -- "$out")/.package-build-tools.XXXXXX")
trap 'rm -rf -- "$stage"' EXIT
dest=$stage/$name
mkdir -p "$dest/bin" "$dest/lib"

real() { basename -- "$(realpath -- "$prefix/bin/$1")"; }
clang_real=$(real clang)
lld_real=$(real ld.lld)
ar_real=$(real llvm-ar)
for file in "$clang_real" "$lld_real" "$ar_real" llvm-profdata llvm-bolt merge-fdata; do
    install -m 755 "$prefix/bin/$file" "$dest/bin/$file"
    strip --strip-all "$dest/bin/$file"
done
link() { [[ $1 == "$2" ]] || ln -s "$1" "$dest/bin/$2"; }
link "$clang_real" clang
link "$clang_real" clang++
link "$lld_real" ld.lld
link "$ar_real" llvm-ar
link "$ar_real" llvm-ranlib

# Clang's resource directory: builtin headers and the profile runtime.
resource=$("$prefix/bin/clang" -print-resource-dir)
resource_rel=${resource#"$prefix"/}
[[ $resource_rel != "$resource" ]] || { echo "resource dir outside prefix: $resource" >&2; exit 1; }
profile_rt=$("$prefix/bin/clang" -print-file-name=libclang_rt.profile.a)
profile_rel=${profile_rt#"$prefix"/}
[[ $profile_rel != "$profile_rt" && -f $profile_rt ]] || {
    echo "profile runtime missing: $profile_rt" >&2
    exit 1
}
mkdir -p "$dest/$resource_rel" "$dest/$(dirname -- "$profile_rel")"
cp -a "$resource/include" "$dest/$resource_rel/include"
cp -a "$profile_rt" "$dest/$profile_rel"
cp -a "$prefix/lib/libbolt_rt_instr.a" "$dest/lib/"
cp -a "$prefix/frust-llvm-tools.stamp" "$dest/"

{
    echo "frust build tools: LLVM $("$prefix/bin/clang" --version | sed -n 1p)"
    echo "built by support/tools/build-llvm-tools.sh; packaged by support/tools/package-build-tools.sh"
    echo "configuration: frust-llvm-tools.stamp"
    if [[ -n $packages ]]; then
        echo "build sysroot (Ubuntu 24.04 packages, support/tools/fetch-noble-sysroot.py):"
        sed 's/^/  /' "$packages"
    fi
    echo "files (sha256):"
    (cd "$dest" && find . -type f ! -name BUILD-INFO | sort | xargs sha256sum | sed 's/^/  /')
} > "$dest/BUILD-INFO"

# Portability checks: relative symlinks, glibc ceiling, only system libraries, no host paths.
if find "$dest" -type l -lname '/*' | grep -q .; then
    echo "absolute symlink in the bundle" >&2
    exit 1
fi
built_tree=$(sed -n 's/^-DCMAKE_INSTALL_PREFIX=//p' "$prefix/frust-llvm-tools.stamp")
sysroot=$(sed -n 's/^-DCMAKE_SYSROOT=//p' "$prefix/frust-llvm-tools.stamp")
for file in "$dest"/bin/*; do
    [[ -L $file ]] && continue
    newest=$(objdump -T "$file" | grep -o 'GLIBC_[0-9.]*' | sed 's/GLIBC_//' | sort -uV | tail -n 1)
    if [[ $(printf '%s\n%s\n' "$newest" "$max_glibc" | sort -V | tail -n 1) != "$max_glibc" ]]; then
        echo "$file needs GLIBC_$newest (> $max_glibc)" >&2
        exit 1
    fi
    needed=$(readelf -d "$file" | sed -n 's/.*(NEEDED).*\[\(.*\)\]/\1/p' | tr '\n' ' ')
    for lib in $needed; do
        case $lib in
            libc.so.6 | libm.so.6 | ld-linux-x86-64.so.2 | libz.so.1 | libzstd.so.1) ;;
            *) echo "$file needs $lib" >&2; exit 1 ;;
        esac
    done
    # One exception: llvm-bolt keeps CMAKE_INSTALL_FULL_LIBDIR as a last-resort runtime lookup
    # (RuntimeLibrary::getLibPathByInstalled). It is unreachable: getLibPathByToolPath, tried
    # first, exits when <bin>/../lib lacks the runtime, and the bundle ships it there.
    allowed=
    [[ $(basename -- "$file") == llvm-bolt ]] && allowed=$built_tree/lib
    for path in "$built_tree" "$sysroot"; do
        [[ -n $path ]] || continue
        if strings -a "$file" | grep -F -- "$path" | grep -vxF -- "${allowed:-//none//}" | grep -q .; then
            echo "$file embeds the host path $path" >&2
            exit 1
        fi
    done
    # LLVM sets $ORIGIN/../lib; only entries outside the bundle would break relocation.
    runpath=$(readelf -d "$file" | sed -n 's/.*R\(UN\)\{0,1\}PATH).*\[\(.*\)\]/\2/p' | tr ':' '\n')
    if grep -v '^\$ORIGIN' <<< "$runpath" | grep -q .; then
        echo "$file has a non-\$ORIGIN RPATH/RUNPATH: $runpath" >&2
        exit 1
    fi
    echo "$(basename -- "$file"): GLIBC <= $newest; NEEDED $needed"
done

tar --sort=name --owner=0 --group=0 --numeric-owner --mtime=@0 -C "$stage" -cf - "$name" |
    zstd -19 -T0 -q -o "$out"
echo "packaged $out ($(stat -c %s "$out") bytes)"
