#!/bin/bash
# Assemble a relocatable toolchain tree from a finished stage-2 build.
# Arguments: <stage2-dir> <cargo-bin> <fcargo-bin> <dest-prefix>
set -euo pipefail
stage2=$1
cargo_bin=$2
fcargo_bin=$3
dest=$4
id=${5:-frust-0ef1bc386-central1}

if [[ -e "$dest" ]]; then
    echo "destination already exists: $dest" >&2
    exit 1
fi
mkdir -p "$dest"
cp -a "$stage2"/. "$dest"/
install -m 755 "$cargo_bin" "$dest/bin/cargo"
install -m 755 "$fcargo_bin" "$dest/bin/fcargo"
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
{"id":"$id","rust_commit":"0ef1bc386059596dada8fb65b50425aab2ea637a","cargo_commit":"3acc79c0c841844675bc53a605922dbfc441e750","llvm_commit":"0a5579eb649d3634a9c30ac873300cc2944fa070","host":"x86_64-unknown-linux-gnu"}
EOF

test -f "$dest/bin/rustc"
test -f "$dest/bin/frustc"
test -f "$dest/bin/cargo"
test -f "$dest/bin/fcargo"
test -f "$dest/bin/rustdoc"
test -f "$dest/bin/frustdoc"
test -s "$dest/manifest.json"
# Same inode: sysroot discovery follows the invoked path.
test "$(stat -c %i "$dest/bin/rustc")" = "$(stat -c %i "$dest/bin/frustc")"
echo "packaged $dest id=$id"
