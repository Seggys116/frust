#!/bin/bash
# Fail when the Cargo patch embeds copies of paths.rs or json_text.rs that differ from the crate sources.
set -euo pipefail
ROOT=$(cd "$(dirname "$0")/.." && pwd)
PATCH=$ROOT/support/patches/0001-frust-central-target.patch
CORE=$ROOT/crates/frust-core/src

# Print the added lines of one new-file hunk with the diff prefix removed.
embedded() {
    awk -v name="b/src/$1" '
        /^diff --git / { active = ($4 == name); body = 0; next }
        active && /^@@ / { body = 1; next }
        active && body && /^\+/ { print substr($0, 2) }
    ' "$PATCH"
}

status=0
check() {
    local file=$1 source=$2 sed_expr=$3
    if ! diff -u <(sed "$sed_expr" "$source") <(embedded "$file") >/dev/null; then
        echo "embedded $file differs from $source" >&2
        status=1
    fi
}

check frust_json_text.rs "$CORE/json_text.rs" 's/^//'
check frust_paths.rs "$CORE/paths.rs" 's/crate::json_text/crate::frust_json_text/'
[ "$status" -eq 0 ] && echo "embedded sources match"
exit "$status"
