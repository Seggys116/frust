#!/bin/bash
# Check out one frozen workload at its locked commit and fetch its dependencies.
# Usage: fetch-cohort.sh <workload> <cohorts-dir> <cargo>
# The tree lands in <cohorts-dir>/<workload>, the layout time-build.sh expects under
# target/compiler-rewrite/cohorts. The Cargo.lock sha256 must match qualification/cohorts.lock.
set -euo pipefail
if [[ $# -ne 3 ]]; then
  echo "usage: fetch-cohort.sh <workload> <cohorts-dir> <cargo>" >&2
  exit 2
fi
root=$(CDPATH= cd -- "$(dirname "$0")/.." && pwd)
workload=$1
dest=$2/$workload
cargo=$3
read -r _ url commit origin want < <(awk -v w="$workload" '$1 == w { print }' "$root/qualification/cohorts.lock")
[[ -n ${commit:-} ]] || { echo "unknown workload $workload" >&2; exit 2; }
if [[ -e $dest ]]; then
  echo "cohort already exists: $dest" >&2
  exit 1
fi

mkdir -p "$dest"
git -C "$dest" init -q
git -C "$dest" fetch -q --depth 1 "$url" "$commit"
git -C "$dest" -c advice.detachedHead=false checkout -q --detach FETCH_HEAD
[[ $origin == upstream ]] || cp "$root/qualification/$origin" "$dest/Cargo.lock"
got=$(sha256sum "$dest/Cargo.lock" | cut -d' ' -f1)
if [[ $got != "$want" ]]; then
  echo "Cargo.lock sha256 mismatch for $workload: want $want got $got" >&2
  exit 1
fi
(cd "$dest" && "$cargo" fetch --locked)
echo "fetched $workload $commit"
