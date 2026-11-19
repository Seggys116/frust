#!/usr/bin/env bash
# Run pinned Rust's opt-dist for a prepared fRust Linux checkout.
set -euo pipefail

if [[ $# != 3 ]]; then
    echo "usage: $0 RUST_SOURCE WORK_DIRECTORY HOST_LLVM_DIRECTORY" >&2
    exit 2
fi
root=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)
source_dir=$(realpath -- "$1")
work=$(realpath -m -- "$2")
llvm=$(realpath -- "$3")
host=x86_64-unknown-linux-gnu

[[ $(uname -s) == Linux && $(uname -m) == x86_64 ]] || {
    echo "this recipe requires Linux x86-64" >&2
    exit 2
}
for tool in llvm-bolt merge-fdata llvm-profdata; do
    "$llvm/bin/$tool" --version
done
clang --version
ld.lld --version
bash "$root/support/verify-pins.sh" "$source_dir"
training=$(git -C "$source_dir" rev-parse HEAD:src/tools/rustc-perf)
[[ -f "$source_dir/src/tools/rustc-perf/Cargo.toml" &&
   $(git -C "$source_dir/src/tools/rustc-perf" rev-parse HEAD) == "$training" ]] || {
    echo "initialize the pinned rustc-perf submodule first" >&2
    exit 2
}

mkdir -p "$work/tmp"
config="$work/bootstrap-optimized.toml"
python3 - "$root/support/bootstrap.toml" "$config" "$work/build" <<'PY'
import json
from pathlib import Path
import sys

source, destination, build = sys.argv[1:]
config = Path(source).read_text().replace(
    "[build]\n", f"[build]\nbuild-dir = {json.dumps(build)}\n"
    'metrics = true\nsubmodules = false\nallocator = "jemalloc"\nprofiler = true\n'
    'cargo-native-static = true\n'
).replace(
    "[rust]\n", "[rust]\ncodegen-units = 1\nbootstrap-override-lld = true\n"
)
config += '\n[target.x86_64-unknown-linux-gnu]\nlinker = "clang"\n'
Path(destination).write_text(config)
PY
export RUST_BOOTSTRAP_CONFIG="$config"
export TMPDIR="$work/tmp"
bootstrap_python="$work/bootstrap-python"
cat > "$bootstrap_python" <<'BASH'
#!/usr/bin/env bash
set -euo pipefail
args=("$@")
# The profiling workloads also need proc_macro and test in the training sysroot.
if [[ ${args[1]:-} == build ]]; then
    for i in "${!args[@]}"; do
        if [[ ${args[i]} == library/std ]]; then
            args[i]=library
        fi
    done
fi
exec python3 "${args[@]}"
BASH
chmod +x "$bootstrap_python"
cd "$source_dir"
python3 x.py build --stage 1 opt-dist
"$work/build/$host/stage1-tools-bin/opt-dist" local \
    --target-triple "$host" --checkout-dir "$source_dir" \
    --llvm-dir "$llvm" --artifact-dir "$work/profiles" --build-dir "$work/build" \
    --python "$bootstrap_python" --build-llvm=false --use-bolt \
    -- python3 x.py dist rustc rust-std cargo
