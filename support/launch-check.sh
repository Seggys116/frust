#!/bin/bash
# Run one ambient-free launch of a packaged toolchain: version output of every tool, a build, a doc build,
# clippy and rustfmt.
# Arguments: <prefix> <work-dir> <log>
set -euo pipefail
prefix=$1
work=$2
log=$3
export PATH="/usr/bin:/bin"
unset RUSTC CARGO CARGO_HOME RUSTUP_HOME FRUST_CARGO FRUST_PROGRESS FRUST_INVOCATION || true
{
    echo "prefix $prefix"
    echo "rustc $($prefix/bin/frustc -Vv)"
    echo "sysroot $($prefix/bin/frustc --print sysroot)"
    echo "cargo $($prefix/bin/cargo -V)"
    echo "rustdoc $($prefix/bin/rustdoc -V)"
} > "$log"
src=$work/hello
if [[ -e "$src" ]]; then
    echo "work source already exists: $src" >&2
    exit 1
fi
mkdir -p "$src/src"
cat > "$src/Cargo.toml" <<'EOF'
[package]
name = "frust_hello"
version = "0.1.0"
edition = "2021"
EOF
cat > "$src/src/main.rs" <<'EOF'
fn main() {
    println!("frust-launch-ok");
}
EOF
target=$work/target
"$prefix/bin/fcargo" build -v --offline --manifest-path "$src/Cargo.toml" --target-dir "$target" >> "$log" 2>&1
got=$("$target/debug/frust_hello")
echo "run $got" >> "$log"
test "$got" = "frust-launch-ok"
"$prefix/bin/frustdoc" --edition 2021 --crate-name frust_hello -o "$work/doc" "$src/src/main.rs" >> "$log" 2>&1
test -s "$work/doc/frust_hello/index.html"
echo "doc ok" >> "$log"
# clippy and rustfmt link the compiler's own crates, so they are built with it and have to work next to
# it. Each is run directly and through fcargo, the way a user reaches it.
{
    echo "clippy-driver $($prefix/bin/clippy-driver -V)"
    echo "cargo-clippy $($prefix/bin/cargo-clippy -V)"
    echo "rustfmt $($prefix/bin/rustfmt -V)"
    echo "cargo-fmt $($prefix/bin/cargo-fmt -V)"
} >> "$log"
"$prefix/bin/fcargo" clippy --offline --manifest-path "$src/Cargo.toml" --target-dir "$target" -- -D warnings >> "$log" 2>&1
echo "clippy ok" >> "$log"
"$prefix/bin/fcargo" fmt --manifest-path "$src/Cargo.toml" -- --check >> "$log" 2>&1
echo "fmt ok" >> "$log"
