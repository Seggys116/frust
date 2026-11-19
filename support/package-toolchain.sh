#!/bin/bash
# Assemble a relocatable toolchain tree from a finished stage-2 build.
# Arguments: <stage2-dir> <cargo-bin> <fcargo-bin> <dest-prefix> [id] [host]
set -euo pipefail
stage2=$1
cargo_bin=$2
fcargo_bin=$3
dest=$4
id=${5:-frust-0ef1bc386-central1}
host=${6:-x86_64-unknown-linux-gnu}

if [[ -e "$dest" ]]; then
    echo "destination already exists: $dest" >&2
    exit 1
fi
mkdir -p "$dest"
cp -a "$stage2"/. "$dest"/
# rustc_private crates are built beside stage2 and are not in its rustlib
# directory. Rustfmt 1.9.0 links them with extern crate. Copy the hashed
# rlibs, rmeta, and shared objects that the sysroot does not already have.
libdir=$dest/lib/rustlib/$host/lib
host_dir=$(dirname "$stage2")
mkdir -p "$libdir"
found_build=0
for rustc_build in \
    "$host_dir/stage2-rustc/$host/release/build" \
    "$host_dir/stage2-rustc/release/build"
do
    if [[ ! -d "$rustc_build" ]]; then
        continue
    fi
    found_build=1
    while IFS= read -r -d '' file; do
        base=$(basename "$file")
        if [[ ! -e "$libdir/$base" ]]; then
            cp -a "$file" "$libdir/$base"
        fi
    done < <(find "$rustc_build" -path '*/out/lib*' \( -name '*.rlib' -o -name '*.rmeta' -o -name '*.so' \) -print0)
done
if [[ "$found_build" -ne 1 ]]; then
    echo "rustc-dev build directory is missing beside $stage2" >&2
    exit 1
fi
install -m 755 "$cargo_bin" "$dest/bin/cargo"
# fcargo is the patched Cargo itself (direct entry); the launcher serves terminals and overrides.
install -m 755 "$fcargo_bin" "$dest/bin/fcargo-ui"
ln "$dest/bin/cargo" "$dest/bin/fcargo"
ln "$dest/bin/rustc" "$dest/bin/frustc"
ln "$dest/bin/rustdoc" "$dest/bin/frustdoc"
printf '%s\n' "$id" > "$dest/frust-toolchain-id"

src=$(CDPATH= cd -- "$(dirname "$0")/.." && pwd)
src=${FRUST_RUST_SRC:-$src/target/compiler-rewrite/src/rust}
notice=$dest/share/doc/licenses
mkdir -p "$notice"
cp "$src/COPYRIGHT" "$src/LICENSE-MIT" "$src/LICENSE-APACHE" "$notice/"
cp "$src/src/tools/cargo/LICENSE-MIT" "$notice/cargo-LICENSE-MIT"
cp "$src/src/tools/cargo/LICENSE-APACHE" "$notice/cargo-LICENSE-APACHE"
cp "$src/src/llvm-project/LICENSE.TXT" "$notice/llvm-LICENSE.TXT"

cat > "$dest/manifest.json" <<EOF
{"id":"$id","rust_commit":"0ef1bc386059596dada8fb65b50425aab2ea637a","cargo_commit":"3acc79c0c841844675bc53a605922dbfc441e750","llvm_commit":"0a5579eb649d3634a9c30ac873300cc2944fa070","host":"$host"}
EOF

test -f "$dest/bin/rustc"
test -f "$dest/bin/frustc"
test -f "$dest/bin/cargo"
test -f "$dest/bin/fcargo"
test -f "$dest/bin/fcargo-ui"
test "$dest/bin/cargo" -ef "$dest/bin/fcargo"
test -f "$dest/bin/rustdoc"
test -f "$dest/bin/frustdoc"
test -s "$dest/manifest.json"
# Same inode: sysroot discovery follows the invoked path.
test "$dest/bin/rustc" -ef "$dest/bin/frustc"
test -e "$libdir/libthin_vec-"*.rlib
span=$(find "$libdir" -name 'librustc_span-*.rlib' -print -quit)
macros=$(find "$libdir" -name 'librustc_macros-*.so' -print -quit)
derive_where=$(find "$libdir" -name 'libderive_where-*.so' -print -quit)
test -n "$span"
test -n "$macros"
test -n "$derive_where"
echo "packaged $dest id=$id"
