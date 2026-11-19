#!/bin/bash
# Prove the packaged compiler still runs fat LTO, honors the requested
# codegen-unit count, and matches warm incremental output to a fresh build.
# This is a correctness check. It does not record a performance sample.
# Usage: prove-codegen.sh <toolchain-prefix>
set -euo pipefail
if [[ $# -ne 1 ]]; then
  echo "usage: prove-codegen.sh <toolchain-prefix>" >&2
  exit 2
fi
root=$(CDPATH= cd -- "$(dirname "$0")/.." && pwd)
prefix=$(CDPATH= cd -- "$1" && pwd)
id=$(tr -d '[:space:]' < "$prefix/frust-toolchain-id")
work=$root/target/compiler-rewrite/parity/$id
if [[ -e "$work" ]]; then
  echo "parity directory already exists: $work" >&2
  exit 1
fi
mkdir -p "$work"
exec > >(tee "$work/prove.log") 2>&1

bins=$prefix/bin
fixture=$root/qualification/parity/fixture
probe=$root/qualification/parity/cgu_probe.rs
export PATH="$bins:/usr/bin:/bin"
unset RUSTC CARGO RUSTFLAGS RUSTUP_TOOLCHAIN FRUST_INVOCATION FRUST_PROGRESS || true
export CARGO_PROFILE_RELEASE_OPT_LEVEL=3
export CARGO_PROFILE_RELEASE_LTO=fat
export CARGO_PROFILE_RELEASE_INCREMENTAL=true
export CARGO_PROFILE_RELEASE_DEBUG=0
export CARGO_PROFILE_RELEASE_STRIP=none
export CARGO_TERM_COLOR=never
export FRUST_PROGRESS=plain
export RUSTC=$bins/frustc
command -v cargo >/dev/null || { echo "FAIL: cargo is not on PATH" >&2; exit 1; }
command -v "$RUSTC" >/dev/null || { echo "FAIL: frustc is not executable" >&2; exit 1; }

fail() { echo "FAIL: $*" >&2; exit 1; }

fat_lto_seconds() {
  local log=$1
  python3 - "$log" <<'PY'
import re, sys
best = 0.0
seen = False
for line in open(sys.argv[1], errors="replace"):
    if "LLVM_fatlto" not in line or "time:" not in line:
        continue
    match = re.search(r"time:\s+([0-9.]+)", line)
    if not match:
        continue
    seen = True
    best = max(best, float(match.group(1)))
if not seen:
    sys.exit("no LLVM_fatlto row")
print(f"{best:.6f}")
PY
}

# Fat LTO writes one object. Each requested codegen unit still has its own
# pre-optimization bitcode when -C save-temps is set.
count_cgu() {
  local compiler=$1 units=$2 dest=$3
  mkdir -p "$dest"
  RUSTC_BOOTSTRAP=1 "$compiler" --edition 2021 --crate-type bin \
    -C opt-level=3 -C lto=fat -C codegen-units="$units" -C debuginfo=0 -C save-temps \
    --out-dir "$dest" "$probe" > "$dest/rustc.log" 2>&1
  [[ -n $(find "$dest" -name '*.o' -print -quit) ]] || fail "$compiler codegen-units=$units emitted no object"
  find "$dest" -name '*.no-opt.bc' | wc -l | tr -d ' '
}

echo "LTO_PRESENCE $(date -Is)"
for name in frustc rustc; do
  mkdir -p "$work/lto-$name"
  RUSTC_BOOTSTRAP=1 "$bins/$name" -Z time-passes --edition 2021 \
    -C opt-level=3 -C lto=fat -C codegen-units=1 -C debuginfo=0 \
    -o "$work/lto-$name/probe" "$probe" > "$work/lto-$name/time-passes.log" 2>&1
  seconds=$(fat_lto_seconds "$work/lto-$name/time-passes.log")
  python3 -c 'import sys; assert float(sys.argv[1]) > 0' "$seconds" \
    || fail "$name LLVM_fatlto was ${seconds}s"
  echo "LLVM_FATLTO $name ${seconds}s"
done

echo "CGU_COUNT $(date -Is)"
unset CARGO_PROFILE_RELEASE_CODEGEN_UNITS || true
frust1=$(count_cgu "$bins/frustc" 1 "$work/cgu-frustc-1")
rust1=$(count_cgu "$bins/rustc" 1 "$work/cgu-rustc-1")
frust8=$(count_cgu "$bins/frustc" 8 "$work/cgu-frustc-8")
rust8=$(count_cgu "$bins/rustc" 8 "$work/cgu-rustc-8")
echo "CGU frustc units=1 files=$frust1"
echo "CGU rustc units=1 files=$rust1"
echo "CGU frustc units=8 files=$frust8"
echo "CGU rustc units=8 files=$rust8"
[[ "$frust1" == "1" && "$rust1" == "1" ]] || fail "codegen-units=1 produced $frust1 and $rust1 pre-opt bitcode files"
[[ "$frust8" == "8" && "$rust8" == "8" ]] || fail "codegen-units=8 produced $frust8 and $rust8 pre-opt bitcode files"

snapshot() {
  local src=$1 dest=$2
  mkdir -p "$dest"
  cp -a "$src/Cargo.toml" "$src/Cargo.lock" "$src/build.rs" "$src/native.c" "$src/src" "$dest/"
  if [[ -d "$src/.cargo" ]]; then
    cp -a "$src/.cargo" "$dest/"
  fi
}

build_release() {
  local dir=$1 target=$2
  (
    cd "$dir"
    export CARGO_TARGET_DIR=$target
    export CARGO_PROFILE_RELEASE_CODEGEN_UNITS=1
    cargo build --release --locked --offline
  )
}

text_bytes() {
  objcopy -O binary --only-section=.text "$1" "$2"
}

run_bin() {
  "$1/release/frust_parity"
}

compare_targets() {
  local warm_target=$1 fresh_target=$2 label=$3
  local warm=$warm_target/release/frust_parity
  local fresh=$fresh_target/release/frust_parity
  text_bytes "$warm" "$work/$label.warm.text"
  text_bytes "$fresh" "$work/$label.fresh.text"
  if ! cmp -s "$work/$label.warm.text" "$work/$label.fresh.text"; then
    fail "$label .text bytes differ"
  fi
  local warm_out fresh_out
  warm_out=$(run_bin "$warm_target")
  fresh_out=$(run_bin "$fresh_target")
  if [[ "$warm_out" != "$fresh_out" ]]; then
    fail "$label stdout $warm_out != $fresh_out"
  fi
  echo "PARITY_OK $label $warm_out" >&2
  printf '%s\n' "$warm_out"
}

native_len=$(wc -c < "$fixture/native.c" | tr -d ' ')
pristine_out="21 2 4 22 44 $native_len"
echo "PRISTINE_EXPECT $pristine_out"

one_case() {
  local name=$1
  local src=$work/src-$name
  snapshot "$fixture" "$src"
  printf '%s\n' "$src"
}

echo "PARITY $(date -Is)"

leaf_src=$(one_case leaf)
build_release "$leaf_src" "$work/leaf-base"
base_out=$(run_bin "$work/leaf-base")
[[ "$base_out" == "$pristine_out" ]] || fail "pristine output $base_out != $pristine_out"
python3 - "$leaf_src/src/main.rs" <<'PY'
import pathlib, sys
path = pathlib.Path(sys.argv[1])
text = path.read_text()
old = "n.wrapping_add(11)"
new = "n.wrapping_add(12)"
if old not in text:
    raise SystemExit("leaf pattern missing")
path.write_text(text.replace(old, new, 1))
PY
build_release "$leaf_src" "$work/leaf-base"
snapshot "$leaf_src" "$work/src-leaf-fresh"
build_release "$work/src-leaf-fresh" "$work/leaf-fresh"
leaf_out=$(compare_targets "$work/leaf-base" "$work/leaf-fresh" leaf)
[[ "$leaf_out" == "22 2 4 22 44 $native_len" ]] || fail "leaf output $leaf_out"

python3 - "$leaf_src/src/main.rs" <<'PY'
import pathlib, sys
path = pathlib.Path(sys.argv[1])
text = path.read_text()
old = "n.wrapping_add(12)"
new = "n.wrapping_add(11)"
if old not in text:
    raise SystemExit("revert pattern missing")
path.write_text(text.replace(old, new, 1))
PY
build_release "$leaf_src" "$work/leaf-base"
snapshot "$leaf_src" "$work/src-revert-fresh"
build_release "$work/src-revert-fresh" "$work/revert-fresh"
revert_out=$(compare_targets "$work/leaf-base" "$work/revert-fresh" revert)
[[ "$revert_out" == "$pristine_out" ]] || fail "revert output $revert_out"

wide_src=$(one_case wide)
build_release "$wide_src" "$work/wide-base"
python3 - "$wide_src/src/main.rs" <<'PY'
import pathlib, sys
path = pathlib.Path(sys.argv[1])
text = path.read_text()
old = "value.as_ref().len().wrapping_add(0)"
new = "value.as_ref().len().wrapping_add(1)"
if old not in text:
    raise SystemExit("generic pattern missing")
text = text.replace(old, new, 1)
old = "self.len().wrapping_add(0)"
new = "self.len().wrapping_add(1)"
if old not in text:
    raise SystemExit("trait pattern missing")
path.write_text(text.replace(old, new, 1))
PY
build_release "$wide_src" "$work/wide-base"
snapshot "$wide_src" "$work/src-wide-fresh"
build_release "$work/src-wide-fresh" "$work/wide-fresh"
wide_out=$(compare_targets "$work/wide-base" "$work/wide-fresh" wide)
[[ "$wide_out" == "21 3 5 22 44 $native_len" ]] || fail "wide output $wide_out"

api_src=$(one_case api)
build_release "$api_src" "$work/api-base"
python3 - "$api_src/src/main.rs" <<'PY'
import pathlib, sys
path = pathlib.Path(sys.argv[1])
text = path.read_text()
old = "pub fn api() -> usize {\n    22\n}"
new = "pub fn api() -> usize {\n    23\n}"
if old not in text:
    raise SystemExit("api pattern missing")
text = text.replace(old, new, 1)
old = "44usize"
new = "45usize"
if old not in text:
    raise SystemExit("macro pattern missing")
path.write_text(text.replace(old, new, 1))
PY
build_release "$api_src" "$work/api-base"
snapshot "$api_src" "$work/src-api-fresh"
build_release "$work/src-api-fresh" "$work/api-fresh"
api_out=$(compare_targets "$work/api-base" "$work/api-fresh" api)
[[ "$api_out" == "21 2 4 23 45 $native_len" ]] || fail "api output $api_out"

native_src=$(one_case native)
build_release "$native_src" "$work/native-base"
printf ' ' >> "$native_src/native.c"
build_release "$native_src" "$work/native-base"
snapshot "$native_src" "$work/src-native-fresh"
build_release "$work/src-native-fresh" "$work/native-fresh"
native_out=$(compare_targets "$work/native-base" "$work/native-fresh" native)
native_next=$((native_len + 1))
[[ "$native_out" == "21 2 4 22 44 $native_next" ]] || fail "native output $native_out"

link_src=$(one_case link)
build_release "$link_src" "$work/link-base"
mkdir -p "$link_src/.cargo"
cat > "$link_src/.cargo/config.toml" <<'EOF'
[build]
rustflags = ["-C", "link-arg=-Wl,--defsym=frust_link_only=1"]
EOF
build_release "$link_src" "$work/link-base"
snapshot "$link_src" "$work/src-link-fresh"
build_release "$work/src-link-fresh" "$work/link-fresh"
link_out=$(compare_targets "$work/link-base" "$work/link-fresh" link)
[[ "$link_out" == "$pristine_out" ]] || fail "linker-only output changed to $link_out"
# grep -q would SIGPIPE nm under pipefail and look like a missing symbol.
if ! nm "$work/link-base/release/frust_parity" | grep -F frust_link_only > "$work/link.nm"; then
  fail "linker symbol missing"
fi
if nm "$work/leaf-base/release/frust_parity" | grep -F frust_link_only > "$work/pristine-link.nm"; then
  fail "pristine binary gained the linker symbol"
fi

stamp_src=$(one_case stamp)
build_release "$stamp_src" "$work/stamp-base"
mtime=$(stat -c %y "$stamp_src/src/main.rs")
python3 - "$stamp_src/src/main.rs" <<'PY'
import pathlib, sys
path = pathlib.Path(sys.argv[1])
text = path.read_text()
old = "n.wrapping_add(11)"
new = "n.wrapping_add(12)"
if old not in text:
    raise SystemExit("stamp pattern missing")
path.write_text(text.replace(old, new, 1))
PY
touch -d "$mtime" "$stamp_src/src/main.rs"
build_release "$stamp_src" "$work/stamp-base"
snapshot "$stamp_src" "$work/src-stamp-fresh"
build_release "$work/src-stamp-fresh" "$work/stamp-fresh"
stamp_out=$(compare_targets "$work/stamp-base" "$work/stamp-fresh" stamp)
[[ "$stamp_out" == "22 2 4 22 44 $native_len" ]] || fail "timestamp edit output $stamp_out"

echo "PROVE_OK $(date -Is)"
