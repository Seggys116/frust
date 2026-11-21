#!/bin/bash
# Validate an installed (or unpacked) fRust toolchain the way a user uses it.
#
# Usage: toolchain-suite.sh <prefix-or-toolchain-dir> <work-dir> [linux|macos-aarch64|macos-x86_64]
#
# The first argument is either an installation prefix (it has current/bin) or a toolchain tree (it has
# bin). The work directory must not exist or must be empty. The third argument names the release bundle
# whose tools and targets are expected; it defaults to the bundle of this host.
#
# Everything runs with no ambient state: PATH is the toolchain's bin plus /usr/bin:/bin, HOME,
# FRUST_HOME and CARGO_HOME are throwaway directories, and every variable that steers Cargo or rustc is
# unset, so a rustup, a ~/.cargo or a config of the person running this cannot make a broken toolchain
# pass. The checks do real work (builds, tests, lints, formatting, docs, rust-analyzer, cross builds and
# links, the installer) and each prints PASS, FAIL or SKIP. A failed check does not stop the run: the end
# lists every failure and the exit status is 1. A SKIP always names its reason and is for the cases where
# this host cannot do the check (no foreign linker, no wasm runtime, no Xcode SDK).
#
# Environment:
#   FRUST_SUITE_ONLINE=1   also run `frust-install target add wasm32-wasip2` against the real network.
#   FRUST_SUITE_TIMEOUT    seconds allowed for each command, default 900.
#   FRUST_SUITE_SCOPE      release (the default) checks a complete release toolchain. built checks the tree
#                          that the build produces before packaging, which has the fRust host toolchain,
#                          clippy and rustfmt but not yet the installer, rust-analyzer, rust-docs or the
#                          extra std targets that package-release.sh adds: only those checks are left out.
#
# Needs no network by default and no tool beyond the system's (a C compiler for host links). It is
# written for bash 3.2, which is what macOS ships.
set -u

if [[ $# -lt 2 || $# -gt 3 ]]; then
    echo "usage: $0 <prefix-or-toolchain-dir> <work-dir> [linux|macos-aarch64|macos-x86_64]" >&2
    exit 2
fi
arg_root=$1
work_arg=$2
bundle_arg=${3:-}

script_dir=$(cd "$(dirname "$0")" && pwd -P)

scope=${FRUST_SUITE_SCOPE:-release}
case $scope in
    release | built) ;;
    *)
        echo "FRUST_SUITE_SCOPE must be release or built, not $scope" >&2
        exit 2
        ;;
esac

# ---------------------------------------------------------------------------------------------------
# Locate the toolchain and prepare the work directory.
# ---------------------------------------------------------------------------------------------------
if [[ -d $arg_root/current/bin ]]; then
    prefix=$(cd "$arg_root" && pwd -P)
    tc=$(cd "$arg_root/current" && pwd -P)
elif [[ -d $arg_root/bin ]]; then
    prefix=
    tc=$(cd "$arg_root" && pwd -P)
else
    echo "no toolchain found: $arg_root has neither current/bin nor bin" >&2
    exit 2
fi
if [[ -e $work_arg && -n $(ls -A "$work_arg" 2> /dev/null) ]]; then
    echo "work directory is not empty: $work_arg" >&2
    exit 2
fi
mkdir -p "$work_arg" || exit 2
work=$(cd "$work_arg" && pwd -P)
bin=$tc/bin
logs=$work/logs
mkdir -p "$logs" "$work/home" "$work/cargo-home" "$work/frust-home" "$work/tools"

case $(uname -s) in
    Linux) host_os=linux ;;
    Darwin) host_os=macos ;;
    *)
        echo "unsupported host: $(uname -s)" >&2
        exit 2
        ;;
esac
machine=$(uname -m)
if [[ -n $bundle_arg ]]; then
    bundle=$bundle_arg
elif [[ $host_os == linux ]]; then
    bundle=linux
elif [[ $machine == arm64 || $machine == aarch64 ]]; then
    bundle=macos-aarch64
else
    bundle=macos-x86_64
fi

case $bundle in
    linux)
        host_triple=x86_64-unknown-linux-gnu
        extra_targets="x86_64-unknown-linux-musl aarch64-unknown-linux-gnu wasm32-unknown-unknown wasm32-wasip1"
        ;;
    macos-aarch64)
        host_triple=aarch64-apple-darwin
        extra_targets="x86_64-apple-darwin aarch64-apple-ios aarch64-apple-ios-sim x86_64-apple-ios aarch64-apple-ios-macabi x86_64-apple-ios-macabi wasm32-unknown-unknown wasm32-wasip1"
        ;;
    macos-x86_64)
        host_triple=x86_64-apple-darwin
        extra_targets="aarch64-apple-darwin aarch64-apple-ios aarch64-apple-ios-sim x86_64-apple-ios aarch64-apple-ios-macabi x86_64-apple-ios-macabi wasm32-unknown-unknown wasm32-wasip1"
        ;;
    *)
        echo "unknown bundle: $bundle (expected linux, macos-aarch64 or macos-x86_64)" >&2
        exit 2
        ;;
esac
[[ $scope == built ]] && extra_targets=
expected_targets="$host_triple $extra_targets"

# The one command that can bound a hang. macOS has no timeout unless coreutils is installed.
timeout_bin=
for candidate in timeout gtimeout; do
    if command -v "$candidate" > /dev/null 2>&1; then
        timeout_bin=$(command -v "$candidate")
        break
    fi
done
limit=${FRUST_SUITE_TIMEOUT:-900}
online=${FRUST_SUITE_ONLINE:-0}

# ---------------------------------------------------------------------------------------------------
# A clean environment.
# ---------------------------------------------------------------------------------------------------
for name in $(env | sed -E -n 's/^((CARGO|RUSTUP|RUSTC|RUSTDOC|RUSTFLAGS|FRUST)[A-Za-z0-9_]*)=.*/\1/p'); do
    unset "$name"
done
unset RUSTFMT RUSTUP_TOOLCHAIN SYSROOT LD_LIBRARY_PATH DYLD_LIBRARY_PATH DYLD_FALLBACK_LIBRARY_PATH
# Programs the suite only runs, never tests (a wasm runtime, a foreign linker, qemu) are looked for in the
# PATH the suite was started with, because the clean PATH below does not have them.
original_path=$PATH
find_host_tool() {
    local dir
    local IFS=:
    for dir in $original_path /usr/local/bin /opt/homebrew/bin; do
        if [[ -n $dir && -x $dir/$1 && ! -d $dir/$1 ]]; then
            printf '%s/%s' "$dir" "$1"
            return 0
        fi
    done
    return 1
}
export HOME=$work/home
export CARGO_HOME=$work/cargo-home
export FRUST_HOME=$work/frust-home
export PATH=$bin:/usr/bin:/bin
export CARGO_TERM_COLOR=never
export CARGO_NET_OFFLINE=true
export CARGO_INCREMENTAL=0
export FRUST_PROGRESS=plain
export TERM=dumb
export RUST_BACKTRACE=0

# ---------------------------------------------------------------------------------------------------
# The check machinery.
# ---------------------------------------------------------------------------------------------------
pass_n=0
fail_n=0
skip_n=0
failed_names=""
log_n=0
LOG_FILE=
started=$SECONDS

# new_log <name> sets LOG_FILE to a fresh numbered log file. It is not a command substitution, so the
# counter survives.
new_log() {
    log_n=$((log_n + 1))
    local safe
    safe=$(printf '%s' "$1" | tr -c 'A-Za-z0-9._-' '_' | cut -c1-80)
    LOG_FILE=$(printf '%s/%03d-%s.log' "$logs" "$log_n" "$safe")
}

report_pass() {
    pass_n=$((pass_n + 1))
    printf 'PASS  %s\n' "$1"
}

report_fail() {
    # name, detail, log
    fail_n=$((fail_n + 1))
    failed_names="$failed_names$1"$'\n'
    printf 'FAIL  %s\n' "$1"
    [[ -n $2 ]] && printf '      %s\n' "$2"
    if [[ -n ${3:-} && -s $3 ]]; then
        printf '      log: %s\n' "$3"
        tail -n 14 "$3" | sed 's/^/      | /'
    fi
}

report_skip() {
    skip_n=$((skip_n + 1))
    printf 'SKIP  %s (%s)\n' "$1" "$2"
}

# Run a command with the time limit. Functions cannot be given to timeout, so this is for programs.
limited() {
    if [[ -n $timeout_bin ]]; then
        "$timeout_bin" "$limit" "$@"
    else
        "$@"
    fi
}

# check <name> <command...>: passes when the command exits 0.
check() {
    local name=$1 log rc=0
    shift
    new_log "$name"
    log=$LOG_FILE
    ( "$@" ) > "$log" 2>&1 || rc=$?
    if [[ $rc -eq 0 ]]; then
        report_pass "$name"
    else
        report_fail "$name" "exit status $rc from: $*" "$log"
    fi
    return $rc
}

# check_match <name> <egrep pattern> <command...>: exit 0 and the output matches.
check_match() {
    local name=$1 pattern=$2 log rc=0
    shift 2
    new_log "$name"
    log=$LOG_FILE
    ( "$@" ) > "$log" 2>&1 || rc=$?
    if [[ $rc -ne 0 ]]; then
        report_fail "$name" "exit status $rc from: $*" "$log"
        return 1
    fi
    if ! grep -Eq -- "$pattern" "$log"; then
        report_fail "$name" "output does not match /$pattern/" "$log"
        return 1
    fi
    report_pass "$name"
}

# check_rejects <name> <egrep pattern> <command...>: a negative test. The command has to fail the way a
# tool fails on bad input (status 1 to 99 or 101, never a crash, a timeout or a missing program) and say
# why in the output.
check_rejects() {
    local name=$1 pattern=$2 log rc=0
    shift 2
    new_log "$name"
    log=$LOG_FILE
    ( "$@" ) > "$log" 2>&1 || rc=$?
    if [[ $rc -eq 0 ]]; then
        report_fail "$name" "expected a failure but the command succeeded: $*" "$log"
        return 1
    fi
    if [[ $rc -ge 100 && $rc -ne 101 ]]; then
        report_fail "$name" "the command did not fail cleanly (status $rc): $*" "$log"
        return 1
    fi
    if ! grep -Eq -- "$pattern" "$log"; then
        report_fail "$name" "failed (status $rc) but the output does not match /$pattern/" "$log"
        return 1
    fi
    report_pass "$name"
}

# check_true <name> <description of what is wrong> <test command...>: a plain test, no log.
check_true() {
    local name=$1 why=$2
    shift 2
    if "$@" > /dev/null 2>&1; then
        report_pass "$name"
    else
        report_fail "$name" "$why" ""
        return 1
    fi
}

in_dir() {
    local dir=$1
    shift
    cd "$dir" && "$@"
}

# Run a program with PATH cut to the system directories, so the only way it finds the tools of the
# toolchain is by looking next to itself.
bare() {
    limited env PATH=/usr/bin:/bin "$@"
}

# Run with a program that has to be limited by the clock.
tcmd() {
    limited "$@"
}

have() {
    command -v "$1" > /dev/null 2>&1
}

contains_word() {
    # contains_word <word> <list...>
    local word=$1 item
    shift
    for item in "$@"; do
        [[ $item == "$word" ]] && return 0
    done
    return 1
}

section() {
    printf '\n== %s\n' "$1"
}

cat << EOF
fRust toolchain suite
  toolchain  $tc
  prefix     ${prefix:-none (a toolchain tree)}
  bundle     $bundle
  workdir    $work
  host       $(uname -sm)
  timeout    ${timeout_bin:-none}
EOF

# ---------------------------------------------------------------------------------------------------
# 1. The programs.
# ---------------------------------------------------------------------------------------------------
section "programs exist and run"
all_bins="cargo fcargo frust rustc frustc rustdoc frustdoc cargo-clippy clippy-driver fclippy rustfmt cargo-fmt frustfmt"
[[ $scope == release ]] && all_bins="$all_bins rust-analyzer frust-install frustup"
for name in $all_bins; do
    if [[ ! -e $bin/$name ]]; then
        report_fail "bin/$name exists" "missing: $bin/$name" ""
        continue
    fi
    if [[ ! -x $bin/$name || -d $bin/$name ]]; then
        report_fail "bin/$name is executable" "not an executable file: $bin/$name" ""
        continue
    fi
    case $name in
        frust-install | frustup) flag=--help ;;
        *) flag=--version ;;
    esac
    check_match "$name $flag" '[A-Za-z0-9]' tcmd "$bin/$name" "$flag"
done

if ! [[ -x $bin/rustc && -x $bin/cargo ]]; then
    echo
    echo "FATAL: rustc or cargo is missing, nothing else can be checked"
    echo "result: $pass_n passed, $fail_n failed, $skip_n skipped"
    exit 1
fi

check_true "rustc on PATH is the toolchain's" "command -v rustc is $(command -v rustc)" \
    test "$(command -v rustc)" = "$bin/rustc"
check_true "no rustup on the suite PATH" "found $(command -v rustup)" test -z "$(command -v rustup)"

section "rustc identity"
vv=$("$bin/rustc" -vV 2> /dev/null)
echo "$vv" | sed 's/^/      /'
want_commit=
want_release=
if [[ -f $script_dir/upstream.lock ]]; then
    want_commit=$(sed -n 's/^rust_commit=//p' "$script_dir/upstream.lock")
    want_release=$(sed -n 's/^rust_release=//p' "$script_dir/upstream.lock")
fi
if [[ -n $want_commit ]]; then
    check_true "rustc -vV commit-hash is $want_commit" "got $(printf '%s' "$vv" | sed -n 's/^commit-hash: //p')" \
        grep -qx "commit-hash: $want_commit" <<< "$vv"
    check_true "rustc -vV release is $want_release" "got $(printf '%s' "$vv" | sed -n 's/^release: //p')" \
        grep -qx "release: $want_release" <<< "$vv"
else
    report_skip "rustc -vV matches upstream.lock" "no support/upstream.lock next to this script"
fi
check_true "rustc -vV host is $host_triple" "got $(printf '%s' "$vv" | sed -n 's/^host: //p')" \
    grep -qx "host: $host_triple" <<< "$vv"
check_true "frustc -vV equals rustc -vV" "frustc and rustc report different versions" \
    test "$("$bin/frustc" -vV 2> /dev/null)" = "$vv"
check_true "rustdoc and frustdoc report the same version" "rustdoc -vV differs from frustdoc -vV" \
    test "$("$bin/rustdoc" -vV 2> /dev/null)" = "$("$bin/frustdoc" -vV 2> /dev/null)"
check_true "rustc --print sysroot is the toolchain" "sysroot is $("$bin/rustc" --print sysroot 2> /dev/null)" \
    test "$(cd "$("$bin/rustc" --print sysroot)" && pwd -P)" = "$tc"
if [[ -f $tc/manifest.json ]]; then
    manifest_commit=$(sed -n 's/.*"rust_commit" *: *"\([0-9a-f]*\)".*/\1/p' "$tc/manifest.json")
    check_true "manifest.json rust_commit matches rustc" "manifest has ${manifest_commit:-none}" \
        grep -qx "commit-hash: $manifest_commit" <<< "$vv"
else
    report_fail "manifest.json exists" "missing: $tc/manifest.json" ""
fi

# ---------------------------------------------------------------------------------------------------
# 2. The manifest, the components and the targets on disk.
# ---------------------------------------------------------------------------------------------------
section "manifest, components and targets agree with the disk"

# json_strings <key>: the strings inside "<key>": [ ... ] of manifest.json, one per line. Works for a
# flat array of strings, the way the manifest writes it.
json_strings() {
    tr -d '\n' < "$tc/manifest.json" \
        | sed -n "s/.*\"$1\" *: *\[\([^]]*\)\].*/\1/p" \
        | grep -o '"[^"]*"' | tr -d '"'
}

disk_targets=
for dir in "$tc"/lib/rustlib/*/; do
    [[ -d $dir/lib ]] || continue
    name=$(basename "$dir")
    # a target has the standard library
    if ls "$dir"/lib/libstd-*.rlib > /dev/null 2>&1 || ls "$dir"/lib/libstd-*.rmeta > /dev/null 2>&1; then
        disk_targets="$disk_targets $name"
    fi
done
disk_targets=${disk_targets# }
echo "      targets on disk: $disk_targets"

for target in $expected_targets; do
    check_true "target $target is installed" "no standard library in $tc/lib/rustlib/$target/lib" \
        contains_word "$target" $disk_targets
done

manifest_targets=$(json_strings targets | tr '\n' ' ')
manifest_targets=${manifest_targets% }
if [[ -f $tc/manifest.json ]] && grep -q '"targets"' "$tc/manifest.json"; then
    for target in $disk_targets; do
        check_true "manifest.json lists target $target" "installed on disk but not in manifest.json targets: $manifest_targets" \
            contains_word "$target" $manifest_targets
    done
    for target in $manifest_targets; do
        check_true "manifest.json target $target exists on disk" "listed in manifest.json but missing from lib/rustlib" \
            contains_word "$target" $disk_targets
    done
elif [[ $scope == built ]]; then
    report_skip "manifest.json lists the targets" "package-release.sh writes them, the tree of the build has none yet"
else
    report_fail "manifest.json lists the targets" "manifest.json has no \"targets\" array" ""
fi

# Component name -> file that proves it is installed. The manifest names a component with or without
# the -preview suffix that the official packages use.
component_file() {
    case $1 in
        cargo) echo "bin/cargo" ;;
        rustc) echo "bin/rustc" ;;
        rustdoc) echo "bin/rustdoc" ;;
        clippy) echo "bin/cargo-clippy" ;;
        rustfmt) echo "bin/rustfmt" ;;
        rust-analyzer) echo "bin/rust-analyzer" ;;
        rust-docs) echo "share/doc/rust/html/index.html" ;;
        rust-src) echo "lib/rustlib/src/rust/library/core/src/lib.rs" ;;
        rust-std) echo "lib/rustlib/$host_triple/lib" ;;
        *) echo "" ;;
    esac
}
canonical_component() {
    local name=${1%-preview}
    case $name in
        rust-std-*) echo rust-std ;;
        *) echo "$name" ;;
    esac
}

if [[ -f $tc/manifest.json ]] && grep -q '"components"' "$tc/manifest.json"; then
    manifest_components=$(json_strings components | tr '\n' ' ')
    echo "      components in manifest: $manifest_components"
    listed=
    for entry in $manifest_components; do
        canonical=$(canonical_component "$entry")
        listed="$listed $canonical"
        proof=$(component_file "$canonical")
        if [[ -z $proof ]]; then
            report_fail "manifest.json component $entry is known" "no rule for component $entry" ""
        else
            check_true "manifest.json component $entry exists on disk" "missing $tc/$proof" test -e "$tc/$proof"
        fi
    done
    for canonical in cargo rustc rustdoc clippy rustfmt rust-analyzer rust-docs; do
        proof=$(component_file "$canonical")
        if [[ -e $tc/$proof ]]; then
            check_true "$canonical is on disk and in manifest.json" "installed ($proof) but not in manifest.json components" \
                contains_word "$canonical" $listed
        fi
    done
elif [[ $scope == built ]]; then
    report_skip "manifest.json lists the components" "package-release.sh writes them, the tree of the build has none yet"
else
    report_fail "manifest.json lists the components" "manifest.json has no \"components\" array" ""
fi

# The documentation component and the libraries the tools need next to the compiler.
if [[ $scope == release ]]; then
    check_true "rust-docs is installed (share/doc/rust/html/index.html)" "missing $tc/share/doc/rust/html/index.html" \
        test -s "$tc/share/doc/rust/html/index.html"
    check_true "rust-docs has the std book (share/doc/rust/html/std/index.html)" "missing $tc/share/doc/rust/html/std/index.html" \
        test -s "$tc/share/doc/rust/html/std/index.html"
fi
if [[ ! -d $tc/lib/rustlib/src/rust/library/std ]]; then
    printf 'NOTE  rust-src is not installed: rust-analyzer cannot show the source of std (not a required component)\n'
fi

# ---------------------------------------------------------------------------------------------------
# 3. Sibling tools are found with no help from PATH.
# ---------------------------------------------------------------------------------------------------
section "tools find their siblings with PATH=/usr/bin:/bin"

# The test workspace: a library with a build script, a path dependency, a proc-macro crate, a vendored
# crate that comes through a directory source (the kind of unit the unit cache stores), unit and
# integration tests, a doctest, a binary and an example. Formatted with rustfmt and clean under clippy,
# so a format or lint failure in it is a real failure of the toolchain.
vendor=$work/vendor
make_workspace() {
    local root=$1
    mkdir -p "$root/core/src" "$root/core/tests" "$root/dep/src" "$root/macros/src" "$root/app/src" "$root/app/examples" "$root/.cargo"
    cat > "$root/.cargo/config.toml" << EOF
[source.crates-io]
replace-with = "vendored"

[source.vendored]
directory = "$vendor"
EOF
    cat > "$root/Cargo.toml" << 'EOF'
[workspace]
members = ["core", "dep", "macros", "app"]
resolver = "3"
EOF
    cat > "$root/dep/Cargo.toml" << 'EOF'
[package]
name = "suite_dep"
version = "0.1.0"
edition = "2024"
EOF
    cat > "$root/dep/src/lib.rs" << 'EOF'
pub fn double(x: u32) -> u32 {
    x * 2
}
EOF
    cat > "$root/macros/Cargo.toml" << 'EOF'
[package]
name = "suite_macros"
version = "0.1.0"
edition = "2024"

[lib]
proc-macro = true
EOF
    cat > "$root/macros/src/lib.rs" << 'EOF'
use proc_macro::TokenStream;

#[proc_macro]
pub fn answer(_input: TokenStream) -> TokenStream {
    "21u32".parse().expect("a literal parses")
}
EOF
    cat > "$root/core/Cargo.toml" << 'EOF'
[package]
name = "suite_core"
version = "0.1.0"
edition = "2024"
build = "build.rs"

[dependencies]
suite_dep = { path = "../dep" }
suite_macros = { path = "../macros" }
suite_vend = "1"
EOF
    cat > "$root/core/build.rs" << 'EOF'
use std::{env, fs, path::Path};

fn main() {
    let out = env::var_os("OUT_DIR").expect("OUT_DIR is set");
    fs::write(
        Path::new(&out).join("generated.rs"),
        "pub const FROM_BUILD_RS: u32 = 7;\n",
    )
    .expect("generated.rs is written");
    println!("cargo::rerun-if-changed=build.rs");
    println!("cargo::rustc-check-cfg=cfg(suite_build_script)");
    println!("cargo::rustc-cfg=suite_build_script");
}
EOF
    cat > "$root/core/src/lib.rs" << 'EOF'
//! The library of the toolchain suite workspace.

include!(concat!(env!("OUT_DIR"), "/generated.rs"));

/// Adds the build script constant, the proc-macro answer and the doubled dependency value.
///
/// ```
/// assert_eq!(suite_core::combine(1), 1 + 7 + 21 + 2);
/// ```
pub fn combine(x: u32) -> u32 {
    x + FROM_BUILD_RS + suite_macros::answer!() + suite_dep::double(1)
}

/// The tag of the vendored crate.
pub fn tag() -> &'static str {
    suite_vend::tag()
}

/// Whether the build script's cfg reached this crate.
pub const fn build_script_cfg() -> bool {
    cfg!(suite_build_script)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn combines() {
        assert_eq!(combine(0), 30);
    }

    #[test]
    fn build_script_cfg_is_set() {
        assert!(build_script_cfg());
    }
}
EOF
    cat > "$root/core/tests/integration.rs" << 'EOF'
#[test]
fn integration_sees_the_library() {
    assert_eq!(suite_core::combine(2), 32);
    assert_eq!(suite_core::tag(), "vendored-ok");
}
EOF
    cat > "$root/app/Cargo.toml" << 'EOF'
[package]
name = "suite_app"
version = "0.1.0"
edition = "2024"

[dependencies]
suite_core = { path = "../core" }
EOF
    cat > "$root/app/src/main.rs" << 'EOF'
fn main() {
    let value = suite_core::combine(0);
    println!("suite-app-ok {value} {}", suite_core::tag());
}
EOF
    cat > "$root/app/examples/demo.rs" << 'EOF'
fn main() {
    println!("suite-demo-ok {}", suite_core::combine(1));
}
EOF
}

# The vendored crate is shared by every workspace of the suite, which is what lets a second workspace
# restore it from the unit cache.
mkdir -p "$vendor/suite_vend/src"
cat > "$vendor/suite_vend/Cargo.toml" << 'EOF'
[package]
name = "suite_vend"
version = "1.0.0"
edition = "2021"
EOF
cat > "$vendor/suite_vend/src/lib.rs" << 'EOF'
pub fn tag() -> &'static str {
    "vendored-ok"
}
EOF
# A directory source is checked against the sha256 of each file it lists, and the unit cache only stores
# the units of a source that is checked.
if have sha256sum; then
    sha256_of() { sha256sum "$1" | cut -d' ' -f1; }
else
    sha256_of() { shasum -a 256 "$1" | cut -d' ' -f1; }
fi
printf '{"files":{"Cargo.toml":"%s","src/lib.rs":"%s"},"package":null}' \
    "$(sha256_of "$vendor/suite_vend/Cargo.toml")" "$(sha256_of "$vendor/suite_vend/src/lib.rs")" \
    > "$vendor/suite_vend/.cargo-checksum.json"

ws=$work/ws
make_workspace "$ws"
echo "      workspace: $ws"

# fcargo, frust and the clippy and rustfmt tools reach rustc, rustdoc, clippy-driver and rustfmt by
# looking next to themselves.
check "bare PATH: fcargo build" in_dir "$ws" bare "$bin/fcargo" build
check "bare PATH: frust build" in_dir "$ws" bare "$bin/frust" build
check "bare PATH: fcargo doc" in_dir "$ws" bare "$bin/fcargo" doc --no-deps
check_match "bare PATH: fcargo clippy" 'Finished' in_dir "$ws" bare "$bin/fcargo" clippy
check_match "bare PATH: frust clippy" 'Finished' in_dir "$ws" bare "$bin/frust" clippy
check "bare PATH: fclippy" in_dir "$ws" bare "$bin/fclippy"
check "bare PATH: fcargo fmt --check" in_dir "$ws" bare "$bin/fcargo" fmt --check
check "bare PATH: frust fmt --check" in_dir "$ws" bare "$bin/frust" fmt --check
check "bare PATH: frustfmt --check" bare "$bin/frustfmt" --edition 2024 --check "$ws/core/src/lib.rs"
check "bare PATH: rustfmt --check" bare "$bin/rustfmt" --edition 2024 --check "$ws/core/src/lib.rs"
check "bare PATH: clippy-driver compiles a file" in_dir "$work/tools" bare "$bin/clippy-driver" "$bin/rustc" --edition 2024 --crate-type lib --out-dir "$work/tools" "$ws/dep/src/lib.rs"
check "bare PATH: rustdoc runs a doctest" in_dir "$ws" bare "$bin/fcargo" test --doc -p suite_core

# Plain cargo, cargo-fmt and cargo-clippy are the stock programs: they take rustc, cargo and rustfmt from
# PATH, as they do in every other toolchain, so a bare PATH is only reported for them and never failed.
note_probe() {
    local name=$1
    shift
    if (cd "$ws" && "$@") > "$logs/note-probe.log" 2>&1; then
        printf 'NOTE  %s works with a bare PATH\n' "$name"
    else
        printf 'NOTE  %s needs the toolchain bin on PATH (stock behaviour, not a failure)\n' "$name"
    fi
}
note_probe "plain cargo build" bare "$bin/cargo" build --target-dir "$work/plain-bare-target"
note_probe "cargo-fmt --check" bare "$bin/cargo-fmt" --check
note_probe "cargo-clippy" bare "$bin/cargo-clippy"

# ---------------------------------------------------------------------------------------------------
# 4. The workspace with fcargo and frust.
# ---------------------------------------------------------------------------------------------------
section "workspace: fcargo and frust, the way a user runs them"
for tool in fcargo frust; do
    check "$tool build" in_dir "$ws" tcmd "$bin/$tool" build --workspace
    check "$tool build --release" in_dir "$ws" tcmd "$bin/$tool" build --release --workspace
    check_match "$tool test" 'test result: ok\. [1-9][0-9]* passed' in_dir "$ws" tcmd "$bin/$tool" test --workspace
    check_match "$tool test runs unit, integration and doc tests" 'Doc-tests suite_core' in_dir "$ws" tcmd "$bin/$tool" test --workspace
    check_match "$tool test --doc (rustdoc doctest)" 'test result: ok\. 1 passed' in_dir "$ws" tcmd "$bin/$tool" test --doc -p suite_core
    check "$tool check" in_dir "$ws" tcmd "$bin/$tool" check --workspace --all-targets
    check_match "$tool run" 'suite-app-ok 30 vendored-ok' in_dir "$ws" tcmd "$bin/$tool" run -p suite_app
    check_match "$tool run --example" 'suite-demo-ok 31' in_dir "$ws" tcmd "$bin/$tool" run -p suite_app --example demo
    check "$tool doc" in_dir "$ws" tcmd "$bin/$tool" doc --workspace --no-deps
done

check_match "fcargo clippy -- -D warnings" 'Finished' in_dir "$ws" tcmd "$bin/fcargo" clippy --workspace --all-targets -- -D warnings
check_match "frust clippy -- -D warnings" 'Finished' in_dir "$ws" tcmd "$bin/frust" clippy --workspace --all-targets -- -D warnings
check_match "fclippy -- -D warnings" 'Finished' in_dir "$ws" tcmd "$bin/fclippy" --workspace --all-targets -- -D warnings
check "fclippy" in_dir "$ws" tcmd "$bin/fclippy"
check_match "cargo clippy -- -D warnings (plain cargo)" 'Finished' in_dir "$ws" tcmd "$bin/cargo" clippy --workspace --all-targets --target-dir "$work/plain-target" -- -D warnings
check "cargo build (plain cargo)" in_dir "$ws" tcmd "$bin/cargo" build --workspace --target-dir "$work/plain-target"
check_match "cargo test (plain cargo)" 'test result: ok\. [1-9][0-9]* passed' in_dir "$ws" tcmd "$bin/cargo" test --workspace --target-dir "$work/plain-target"

check "fcargo fmt --check" in_dir "$ws" tcmd "$bin/fcargo" fmt --all --check
check "frust fmt --check" in_dir "$ws" tcmd "$bin/frust" fmt --all --check
check "frustfmt --check on the workspace sources" tcmd "$bin/frustfmt" --edition 2024 --check "$ws/core/src/lib.rs" "$ws/core/build.rs" "$ws/dep/src/lib.rs" "$ws/macros/src/lib.rs" "$ws/app/src/main.rs"
check "rustfmt --edition 2024 --check" tcmd "$bin/rustfmt" --edition 2024 --check "$ws/core/src/lib.rs"
check "cargo-fmt --check" in_dir "$ws" tcmd "$bin/cargo-fmt" --all --check

# ---------------------------------------------------------------------------------------------------
# 5. Negative tests: lints and formatting have to be able to fail.
# ---------------------------------------------------------------------------------------------------
section "negative tests: clippy and rustfmt must reject bad code"
neg=$work/neg
mkdir -p "$neg/lint/src" "$neg/fix/src" "$neg/fmt/src"
cat > "$neg/lint/Cargo.toml" << 'EOF'
[package]
name = "neg_lint"
version = "0.1.0"
edition = "2024"

[workspace]
EOF
cat > "$neg/lint/src/lib.rs" << 'EOF'
pub fn total(values: &Vec<u32>) -> u32 {
    return values.iter().sum();
}
EOF
cat > "$neg/fix/Cargo.toml" << 'EOF'
[package]
name = "neg_fix"
version = "0.1.0"
edition = "2024"

[workspace]
EOF
cat > "$neg/fix/src/lib.rs" << 'EOF'
pub fn first(values: &[u32]) -> u32 {
    return values[0];
}
EOF
cat > "$neg/fmt/Cargo.toml" << 'EOF'
[package]
name = "neg_fmt"
version = "0.1.0"
edition = "2024"

[workspace]
EOF
printf 'fn main(){let   x=1 ;println!("{}",x)}\n' > "$neg/fmt/src/main.rs"

check "clippy lets the lint crate build without -D warnings" in_dir "$neg/lint" tcmd "$bin/fcargo" clippy
check_rejects "fcargo clippy -- -D warnings rejects a lint" 'clippy::|could not compile' in_dir "$neg/lint" tcmd "$bin/fcargo" clippy -- -D warnings
check_rejects "frust clippy -- -D warnings rejects a lint" 'clippy::|could not compile' in_dir "$neg/lint" tcmd "$bin/frust" clippy -- -D warnings
check_rejects "fclippy -- -D warnings rejects a lint" 'clippy::|could not compile' in_dir "$neg/lint" tcmd "$bin/fclippy" -- -D warnings
check_rejects "cargo clippy -- -D warnings rejects a lint" 'clippy::|could not compile' in_dir "$neg/lint" tcmd "$bin/cargo" clippy -- -D warnings
check_match "lint output names the lints" 'needless_return' in_dir "$neg/lint" tcmd "$bin/fcargo" clippy

check "cargo clippy --fix --allow-dirty" in_dir "$neg/fix" tcmd "$bin/cargo" clippy --fix --allow-dirty --allow-no-vcs
check_true "clippy --fix rewrote the source" "src/lib.rs still contains return" test -n "$(grep -L 'return' "$neg/fix/src/lib.rs")"
check_match "fixed crate passes clippy -D warnings" 'Finished' in_dir "$neg/fix" tcmd "$bin/fcargo" clippy -- -D warnings

check_rejects "fcargo fmt --check rejects unformatted code" 'Diff in' in_dir "$neg/fmt" tcmd "$bin/fcargo" fmt --check
check_rejects "frust fmt --check rejects unformatted code" 'Diff in' in_dir "$neg/fmt" tcmd "$bin/frust" fmt --check
check_rejects "cargo-fmt --check rejects unformatted code" 'Diff in' in_dir "$neg/fmt" tcmd "$bin/cargo-fmt" --check
check_rejects "frustfmt --check rejects unformatted code" 'Diff in' tcmd "$bin/frustfmt" --edition 2024 --check "$neg/fmt/src/main.rs"
check_rejects "rustfmt --edition 2024 --check rejects unformatted code" 'Diff in' tcmd "$bin/rustfmt" --edition 2024 --check "$neg/fmt/src/main.rs"
check "fcargo fmt rewrites the file" in_dir "$neg/fmt" tcmd "$bin/fcargo" fmt
check_true "fcargo fmt produced the formatted source" "src/main.rs is not formatted" \
    test "$(cat "$neg/fmt/src/main.rs")" = "$(printf 'fn main() {\n    let x = 1;\n    println!("{}", x)\n}')"
check "frustfmt --check passes after fcargo fmt" tcmd "$bin/frustfmt" --edition 2024 --check "$neg/fmt/src/main.rs"
printf 'fn   main ( ) { }\n' > "$neg/fmt/src/other.rs"
check "rustfmt --edition 2024 formats a file" tcmd "$bin/rustfmt" --edition 2024 "$neg/fmt/src/other.rs"
check_true "rustfmt wrote the formatted file" "other.rs is not formatted" test "$(cat "$neg/fmt/src/other.rs")" = "fn main() {}"

# ---------------------------------------------------------------------------------------------------
# 6. Central target directory and the unit cache.
# ---------------------------------------------------------------------------------------------------
section "central target directory and unit cache"
where_dir=$(cd "$ws" && "$bin/frust" where 2> "$logs/where.err")
where_rc=$?
if [[ $where_rc -ne 0 || -z $where_dir ]]; then
    report_fail "frust where prints the target directory" "exit status $where_rc, output '$where_dir', $(head -c 300 "$logs/where.err")" ""
else
    report_pass "frust where prints the target directory ($where_dir)"
    check_true "frust where names an existing directory after a build" "$where_dir does not exist" test -d "$where_dir"
    check_true "the central target directory holds the build output" "no debug/suite_app in $where_dir" test -x "$where_dir/debug/suite_app"
    check_true "the central target directory is under FRUST_HOME" "$where_dir is outside $FRUST_HOME" \
        test "${where_dir#"$FRUST_HOME"/}" != "$where_dir"
fi
check_true "fcargo doc wrote the crate documentation" "no doc/suite_core/index.html in $where_dir" test -s "$where_dir/doc/suite_core/index.html"
check_true "the workspace has no target/ directory of its own" "found $ws/target" test ! -e "$ws/target"
check_true "plain cargo kept its build next to the explicit target dir" "plain cargo build is missing" test -d "$work/plain-target/debug"

# A second workspace at another path: the vendored crate comes from the unit cache. The first
# workspace's build stored it (FRUST_UNIT_CACHE_DEBUG prints the decisions).
ws2=$work/ws2
make_workspace "$ws2"
new_log "unit cache: clean workspace restores units"
log=$LOG_FILE
( cd "$ws2" && FRUST_UNIT_CACHE_DEBUG=1 limited "$bin/fcargo" build --workspace ) > "$log" 2>&1
rc=$?
if [[ $rc -ne 0 ]]; then
    report_fail "unit cache: a second clean workspace builds" "exit status $rc" "$log"
elif grep -Eq 'frust-unit-cache: hit [0-9a-f]+ suite_vend' "$log"; then
    report_pass "unit cache: a second clean workspace restored suite_vend from the cache"
else
    report_fail "unit cache: a second clean workspace restored suite_vend from the cache" \
        "no 'frust-unit-cache: hit [0-9a-f]+ suite_vend' line: $(grep -c 'frust-unit-cache' "$log") cache lines" "$log"
fi
check_true "the second workspace has its own central target directory" "ws2 target is $(cd "$ws2" && "$bin/frust" where 2> /dev/null)" \
    test "$(cd "$ws2" && "$bin/frust" where 2> /dev/null)" != "$where_dir"
check_match "the restored build runs" 'suite-app-ok 30 vendored-ok' in_dir "$ws2" tcmd "$bin/fcargo" run -p suite_app

# ---------------------------------------------------------------------------------------------------
# 7. rust-analyzer.
# ---------------------------------------------------------------------------------------------------
section "rust-analyzer"
if [[ $scope == release ]]; then
    check_match "rust-analyzer --version" 'rust-analyzer' tcmd "$bin/rust-analyzer" --version
    check_match "rust-analyzer analysis-stats loads and analyses the workspace" 'Item Collection|Inference|Total:|Database loaded' \
        tcmd "$bin/rust-analyzer" analysis-stats "$ws"
    # rust-analyzer expands proc macros through a server it looks for in <sysroot>/libexec. Both are built
    # from source with the compiler of the tree (the official ones link the official librustc_driver), and
    # without the server every proc-macro call in an editor is an error.
    check_true "rust-analyzer's proc-macro server is installed (libexec/rust-analyzer-proc-macro-srv)" \
        "missing $tc/libexec/rust-analyzer-proc-macro-srv, rust-analyzer cannot expand proc macros" \
        test -x "$tc/libexec/rust-analyzer-proc-macro-srv"
    check_match "rust-analyzer's proc-macro server runs from the tree" 'rust-analyzer-proc-macro-srv' \
        env RUST_ANALYZER_INTERNALS_DO_NOT_USE=1 "$tc/libexec/rust-analyzer-proc-macro-srv" --version
    # Without the standard library source rust-analyzer reports errors for core macros like cfg!, so the
    # diagnostics scan means something only when rust-src is installed.
    if [[ -d $tc/lib/rustlib/src/rust/library/core ]]; then
        check "rust-analyzer diagnostics finds no error in the workspace" tcmd "$bin/rust-analyzer" diagnostics "$ws"
    else
        report_skip "rust-analyzer diagnostics" "rust-src is not installed, so the scan would report errors in std macros"
    fi
else
    report_skip "rust-analyzer" "the tree of the build has no rust-analyzer yet"
fi

# ---------------------------------------------------------------------------------------------------
# 8. Every target: print cfg, build a library for it, link where this host can.
# ---------------------------------------------------------------------------------------------------
section "targets: cfg, library build, link"
target_list=$work/target-list.txt
"$bin/rustc" --print target-list > "$target_list" 2> /dev/null
all_targets=$expected_targets
for target in $disk_targets; do
    contains_word "$target" $all_targets || all_targets="$all_targets $target"
done
for target in $all_targets; do
    check_true "rustc --print target-list has $target" "$target is not a target of this rustc" grep -qx "$target" "$target_list"
    check_match "rustc --print cfg --target $target" 'target_arch="' tcmd "$bin/rustc" --print cfg --target "$target"
    check_true "$target has libcore and libstd" "lib/rustlib/$target/lib lacks libcore or libstd" \
        sh -c "ls '$tc/lib/rustlib/$target/lib'/libcore-*.rlib '$tc/lib/rustlib/$target/lib'/libstd-*.rlib"
    check "frust build --target $target (library with build script, proc macro and dependencies)" \
        in_dir "$ws" tcmd "$bin/frust" build --target "$target" -p suite_core
done

# Small sources for linking.
cat > "$work/tools/hello.rs" << 'EOF'
fn main() {
    println!("suite-link-ok {}", std::env::consts::ARCH);
}
EOF
cat > "$work/tools/addlib.rs" << 'EOF'
#[unsafe(no_mangle)]
pub extern "C" fn add(a: i32, b: i32) -> i32 {
    a + b
}
EOF
cat > "$work/tools/stat.rs" << 'EOF'
#[unsafe(no_mangle)]
pub extern "C" fn suite_stat(a: i32) -> i32 {
    a + 1
}
EOF
cat > "$work/tools/run-wasm.js" << 'EOF'
const fs = require("fs");
WebAssembly.instantiate(fs.readFileSync(process.argv[2]), {}).then((m) => {
  console.log("suite-wasm-ok add=" + m.instance.exports.add(40, 2));
});
EOF
cat > "$work/tools/run-wasi.js" << 'EOF'
const fs = require("fs");
const { WASI } = require("node:wasi");
const wasi = new WASI({ version: "preview1", args: ["hello"], env: {} });
WebAssembly.compile(fs.readFileSync(process.argv[2]))
  .then((module) => WebAssembly.instantiate(module, wasi.getImportObject()))
  .then((instance) => wasi.start(instance));
EOF

section "links that this host can do"
out=$work/link
mkdir -p "$out"

# Link <target> <crate-type> <source> <output name> [extra rustc args...]
rustc_for() {
    local target=$1 ctype=$2 src=$3 outdir=$4
    shift 4
    mkdir -p "$outdir"
    tcmd "$bin/rustc" --edition 2024 --target "$target" --crate-type "$ctype" -C opt-level=1 --out-dir "$outdir" "$@" "$src"
}

# A wasm runtime, when the host has one. node is on the GitHub runners.
wasm_runner=
wasm_runner_path=
for candidate in wasmtime wasmer node; do
    if wasm_runner_path=$(find_host_tool "$candidate"); then
        wasm_runner=$candidate
        break
    fi
done

run_wasm_cdylib() {
    case $wasm_runner in
        wasmtime) "$wasm_runner_path" run --invoke add "$1" 40 2 ;;
        wasmer) "$wasm_runner_path" run "$1" --invoke add 40 2 ;;
        node) "$wasm_runner_path" "$work/tools/run-wasm.js" "$1" ;;
    esac
}
run_wasi_bin() {
    case $wasm_runner in
        wasmtime) "$wasm_runner_path" run "$1" ;;
        wasmer) "$wasm_runner_path" run "$1" ;;
        node) "$wasm_runner_path" --no-warnings "$work/tools/run-wasi.js" "$1" ;;
    esac
}

target_present() {
    contains_word "$1" $disk_targets
}

if target_present wasm32-unknown-unknown; then
    check "wasm32-unknown-unknown cdylib links with rust-lld" rustc_for wasm32-unknown-unknown cdylib "$work/tools/addlib.rs" "$out/wasm-cdylib"
    check_match "wasm32-unknown-unknown cdylib is a wasm module" 'a +s +m' sh -c "head -c 4 '$out/wasm-cdylib/addlib.wasm' | od -c"
    check_true "wasm32-unknown-unknown cdylib exports add" "the module does not export add" grep -aq 'add' "$out/wasm-cdylib/addlib.wasm"
    if [[ -n $wasm_runner ]]; then
        if [[ $wasm_runner == node ]]; then
            check_match "wasm32-unknown-unknown cdylib runs under node: add(40,2)" 'suite-wasm-ok add=42' run_wasm_cdylib "$out/wasm-cdylib/addlib.wasm"
        else
            check_match "wasm32-unknown-unknown cdylib runs under $wasm_runner: add(40,2)" '42' run_wasm_cdylib "$out/wasm-cdylib/addlib.wasm"
        fi
    else
        report_skip "run the wasm32-unknown-unknown cdylib" "no wasmtime, wasmer or node on this host"
    fi
    check "wasm32-unknown-unknown staticlib" rustc_for wasm32-unknown-unknown staticlib "$work/tools/stat.rs" "$out/wasm-static"
fi
if target_present wasm32-wasip1; then
    check "wasm32-wasip1 binary links with rust-lld" rustc_for wasm32-wasip1 bin "$work/tools/hello.rs" "$out/wasip1"
    if [[ -n $wasm_runner ]]; then
        check_match "wasm32-wasip1 binary runs under $wasm_runner" 'suite-link-ok wasm32' run_wasi_bin "$out/wasip1/hello.wasm"
    else
        report_skip "run the wasm32-wasip1 binary" "no wasmtime, wasmer or node on this host"
    fi
fi

if [[ $host_os == linux ]]; then
    if target_present x86_64-unknown-linux-musl; then
        check "x86_64-unknown-linux-musl binary links with the self-contained crt" rustc_for x86_64-unknown-linux-musl bin "$work/tools/hello.rs" "$out/musl"
        if [[ $machine == x86_64 ]]; then
            check_match "x86_64-unknown-linux-musl binary runs" 'suite-link-ok x86_64' "$out/musl/hello"
            if have ldd; then
                check_match "x86_64-unknown-linux-musl binary is static" 'not a dynamic executable|statically linked' sh -c "ldd '$out/musl/hello' 2>&1; true"
            fi
        else
            report_skip "run the x86_64-unknown-linux-musl binary" "host is $machine"
        fi
    fi
    if target_present aarch64-unknown-linux-gnu; then
        check "aarch64-unknown-linux-gnu staticlib" rustc_for aarch64-unknown-linux-gnu staticlib "$work/tools/stat.rs" "$out/aarch64-static"
        if aarch64_gcc=$(find_host_tool aarch64-linux-gnu-gcc); then
            check "aarch64-unknown-linux-gnu binary links with aarch64-linux-gnu-gcc" \
                rustc_for aarch64-unknown-linux-gnu bin "$work/tools/hello.rs" "$out/aarch64" -C linker="$aarch64_gcc"
            if qemu=$(find_host_tool qemu-aarch64); then
                check_match "aarch64-unknown-linux-gnu binary runs under qemu-aarch64" 'suite-link-ok aarch64' \
                    "$qemu" -L /usr/aarch64-linux-gnu "$out/aarch64/hello"
            else
                report_skip "run the aarch64-unknown-linux-gnu binary" "no qemu-aarch64 on this host"
            fi
        else
            report_skip "link an aarch64-unknown-linux-gnu binary" "no aarch64-linux-gnu-gcc (a foreign linker) on this host"
        fi
    fi
else
    # macOS: the other architecture of macOS, and the iOS family. Links go through the host cc and the
    # SDKs that Xcode provides, found with xcrun.
    other_darwin=x86_64-apple-darwin
    [[ $host_triple == x86_64-apple-darwin ]] && other_darwin=aarch64-apple-darwin
    other_arch=x86_64
    [[ $other_darwin == aarch64-apple-darwin ]] && other_arch=arm64
    if target_present "$other_darwin"; then
        check "$other_darwin binary links (cross-architecture)" rustc_for "$other_darwin" bin "$work/tools/hello.rs" "$out/darwin-other"
        if have lipo; then
            check_match "$other_darwin binary is $other_arch" "$other_arch" lipo -archs "$out/darwin-other/hello"
        fi
        if [[ $other_arch == x86_64 ]]; then
            if arch -x86_64 /usr/bin/true > /dev/null 2>&1; then
                check_match "$other_darwin binary runs under Rosetta" 'suite-link-ok x86_64' arch -x86_64 "$out/darwin-other/hello"
            else
                report_skip "run the $other_darwin binary" "Rosetta is not installed on this host"
            fi
        else
            report_skip "run the $other_darwin binary" "an arm64 binary cannot run on an Intel host"
        fi
    fi
    check "$host_triple binary links and runs" rustc_for "$host_triple" bin "$work/tools/hello.rs" "$out/darwin-host"
    check_match "$host_triple binary runs" 'suite-link-ok' "$out/darwin-host/hello"

    sdk_ok() {
        xcrun --sdk "$1" --show-sdk-path > /dev/null 2>&1
    }
    ios_arch() {
        case $1 in
            aarch64-*) echo arm64 ;;
            x86_64-*) echo x86_64 ;;
        esac
    }
    for target in aarch64-apple-ios aarch64-apple-ios-sim x86_64-apple-ios aarch64-apple-ios-macabi x86_64-apple-ios-macabi; do
        target_present "$target" || continue
        case $target in
            *-ios-sim | x86_64-apple-ios) sdk=iphonesimulator ;;
            *-macabi) sdk=macosx ;;
            *) sdk=iphoneos ;;
        esac
        arch=$(ios_arch "$target")
        check "$target staticlib" rustc_for "$target" staticlib "$work/tools/stat.rs" "$out/$target-static"
        if ! have xcrun; then
            report_skip "link $target" "no xcrun on this host"
        elif ! sdk_ok "$sdk"; then
            report_skip "link $target" "the $sdk SDK is not installed (xcrun --sdk $sdk fails)"
        else
            check "$target binary links against the $sdk SDK" rustc_for "$target" bin "$work/tools/hello.rs" "$out/$target-bin"
            check "$target cdylib links against the $sdk SDK" rustc_for "$target" cdylib "$work/tools/stat.rs" "$out/$target-dylib"
            if have lipo; then
                check_match "$target binary is $arch" "$arch" lipo -archs "$out/$target-bin/hello"
            fi
        fi
    done
    # The macOS-only proofs (signatures, relocation, deployment targets, every Apple target built with
    # frust) live in their own file, which runs its section when sourced.
    # shellcheck source=support/toolchain-suite-macos.sh
    source "$script_dir/toolchain-suite-macos.sh"
fi

# ---------------------------------------------------------------------------------------------------
# 9. The installer.
# ---------------------------------------------------------------------------------------------------
section "frust-install and frustup"
if [[ $scope == release ]]; then
    # A toolchain tree has no installation prefix around it, so one is made of links: the installer then
    # works on the very tree under test.
    installer_prefix=$prefix
    if [[ -z $installer_prefix ]]; then
        installer_prefix=$work/prefix
        id=$(sed -n 's/.*"id" *: *"\([^"]*\)".*/\1/p' "$tc/manifest.json" 2> /dev/null)
        id=${id:-suite-tree}
        mkdir -p "$installer_prefix/versions"
        ln -s "$tc" "$installer_prefix/versions/$id"
        ln -s "versions/$id" "$installer_prefix/current"
        {
            printf '{\n  "current": "%s",\n  "previous": null\n}\n' "$id"
        } > "$installer_prefix/state.json"
    fi
    installed_list=$work/installed-targets.txt
    check "frust-install target list --installed" in_dir "$work" sh -c "'$bin/frust-install' target list --installed --prefix '$installer_prefix' > '$installed_list'"
    for target in $expected_targets; do
        check_true "frust-install target list --installed shows $target" "not in $(tr '\n' ' ' < "$installed_list")" grep -qw -- "$target" "$installed_list"
    done
    check "frustup target list --installed" in_dir "$work" tcmd "$bin/frustup" target list --installed --prefix "$installer_prefix"
    check_match "frustup show names the active toolchain" '[A-Za-z0-9]' in_dir "$work" tcmd "$bin/frustup" show --prefix "$installer_prefix"
    check_match "frust-install show names the active toolchain" '[A-Za-z0-9]' in_dir "$work" tcmd "$bin/frust-install" show --prefix "$installer_prefix"
    check_match "frustup which cargo-clippy finds the clippy of the toolchain" 'cargo-clippy' in_dir "$work" tcmd "$bin/frustup" which cargo-clippy --prefix "$installer_prefix"

    if [[ $online == 1 ]]; then
        export CARGO_NET_OFFLINE=false
        new=wasm32-wasip2
        check "frust-install target add $new (network)" in_dir "$work" tcmd "$bin/frust-install" target add "$new" --prefix "$installer_prefix"
        check_true "$new is installed in the toolchain" "no lib/rustlib/$new/lib" test -d "$tc/lib/rustlib/$new/lib"
        check "frust build --target $new" in_dir "$ws" tcmd "$bin/frust" build --target "$new" -p suite_core
        check_match "frust-install target list --installed shows $new" "$new" in_dir "$work" tcmd "$bin/frust-install" target list --installed --prefix "$installer_prefix"
        check "frust-install target remove $new" in_dir "$work" tcmd "$bin/frust-install" target remove "$new" --prefix "$installer_prefix"
        check_true "$new is gone from the toolchain" "lib/rustlib/$new still exists" test ! -e "$tc/lib/rustlib/$new"
        export CARGO_NET_OFFLINE=true
    else
        report_skip "frust-install target add wasm32-wasip2" "FRUST_SUITE_ONLINE=1 is not set, the suite uses no network by default"
    fi
else
    report_skip "frust-install and frustup" "the tree of the build has no installer yet"
fi

# ---------------------------------------------------------------------------------------------------
# Result.
# ---------------------------------------------------------------------------------------------------
printf '\n== result\n'
printf 'checks: %d passed, %d failed, %d skipped in %d seconds\n' "$pass_n" "$fail_n" "$skip_n" "$((SECONDS - started))"
if [[ $fail_n -gt 0 ]]; then
    printf 'FAILED CHECKS:\n'
    printf '%s' "$failed_names" | sed 's/^/  - /'
    printf 'logs are in %s\n' "$logs"
    exit 1
fi
printf 'every check passed\n'
