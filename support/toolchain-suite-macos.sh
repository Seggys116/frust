#!/bin/bash
# The macOS part of support/toolchain-suite.sh. Sourced by it on macOS, after its check machinery
# (check, check_match, check_true, report_*, section, tcmd, have, contains_word) and its variables
# (bin, tc, work, bundle, host_triple, disk_targets, machine, script_dir) exist. Not run alone.
# Written for bash 3.2, which is what macOS ships.
#
# What it proves, on the toolchain as installed (so after tar extraction and the installer):
#   1. Every Mach-O program of the toolchain has the architecture of the host, a valid code
#      signature (Apple Silicon only runs signed code, and Apple's linker signs ad hoc), only
#      system, @rpath and relative dependencies that exist, the macOS platform, and the deployment
#      target the build used (11.0 on arm64, 10.12 on x86_64).
#   2. A copy of the toolchain in a path with spaces and non-ASCII characters still runs rustfmt,
#      clippy-driver, cargo-clippy and fclippy: the rpaths are relative, nothing is tied to the
#      path the archive was built or unpacked in, and copying kept the signatures.
#   3. frust builds a package of every crate type (lib, staticlib, cdylib, bin) for each bundled
#      Apple target. The results are Mach-O files of the right architecture and platform (iOS, iOS
#      simulator, Mac Catalyst, macOS), signed where the linker signs. A stripped release build still
#      has a valid signature. fclippy and frust check run on every Apple target too.
# The SDKs come from Xcode (xcrun). On GitHub Actions or with CI=true a missing SDK is a failure,
# elsewhere the checks that need it are skipped with the reason.

# The variables and functions named above come from the script that sources this file.
# shellcheck disable=SC2154
# shellcheck source=support/package-tools.sh
[[ -f $script_dir/package-tools.sh ]] && source "$script_dir/package-tools.sh"

# The deployment target the plain macOS recipe builds with (.github/scripts/build-toolchain.sh):
# 11.0 on aarch64-apple-darwin, 10.12 on x86_64-apple-darwin.
mac_deployment=10.12
mac_arch=x86_64
if [[ $host_triple == aarch64-apple-darwin ]]; then
    mac_deployment=11.0
    mac_arch=arm64
fi

# mac_unavailable <name> <reason>: a check that needs something this host lacks. A failure in CI
# (a runner that lost its Xcode would otherwise pass without testing iOS), a skip elsewhere.
mac_unavailable() {
    if [[ ${GITHUB_ACTIONS:-} == true || ${CI:-} == true ]]; then
        report_fail "$1" "$2 (CI must have it)" ""
    else
        report_skip "$1" "$2"
    fi
}

# Print the platform of the Mach-O file $1: MACOS, IOS, IOSSIMULATOR, MACCATALYST and so on, from
# LC_BUILD_VERSION (otool prints a name or the number), or MACOS-LEGACY and IOS-LEGACY for the
# older LC_VERSION_MIN commands.
mac_platform() {
    local raw
    raw=$(otool -l "$1" 2> /dev/null | awk '
        $1 == "cmd" && $2 == "LC_BUILD_VERSION" { b = 1; next }
        $1 == "cmd" && $2 == "LC_VERSION_MIN_MACOSX" { print "MACOS-LEGACY"; exit }
        $1 == "cmd" && $2 == "LC_VERSION_MIN_IPHONEOS" { print "IOS-LEGACY"; exit }
        b && $1 == "platform" { print $2; exit }')
    case $raw in
        1) echo MACOS ;;
        2) echo IOS ;;
        3) echo TVOS ;;
        4) echo WATCHOS ;;
        6) echo MACCATALYST ;;
        7) echo IOSSIMULATOR ;;
        *) echo "$raw" ;;
    esac
}

# Print the minimum OS version of the Mach-O file $1 (minos, or version of a LC_VERSION_MIN command).
mac_minos() {
    otool -l "$1" 2> /dev/null | awk '
        $1 == "cmd" && ($2 == "LC_BUILD_VERSION" || $2 == "LC_VERSION_MIN_MACOSX" || $2 == "LC_VERSION_MIN_IPHONEOS") { s = 1; next }
        s && ($1 == "minos" || $1 == "version") { print $2; exit }'
}

# mac_version_le <a> <b>: success when dotted version a is not newer than b.
mac_version_le() {
    awk -v a="$1" -v b="$2" 'BEGIN {
        split(a, x, "."); split(b, y, ".")
        for (i = 1; i <= 3; i++) {
            u = x[i] + 0; v = y[i] + 0
            if (u < v) exit 0
            if (u > v) exit 1
        }
        exit 0
    }'
}

# mac_macho_ok <file> <arch> <platform-pattern> [max-minos]: prints what it found and fails on the
# first thing that is wrong. The platform pattern is an egrep alternation such as 'IOS|IOS-LEGACY'.
mac_macho_ok() {
    local f=$1 want_arch=$2 want_platform=$3 max_minos=${4:-} archs plat minos
    archs=$(lipo -archs "$f" 2>&1) || { echo "lipo -archs failed on $f: $archs"; return 1; }
    echo "$f: archs=$archs"
    [[ $archs == "$want_arch" ]] || { echo "architecture is $archs, expected $want_arch"; return 1; }
    plat=$(mac_platform "$f")
    minos=$(mac_minos "$f")
    echo "platform=${plat:-none} minos=${minos:-none}"
    printf '%s\n' "$plat" | grep -Eqx "$want_platform" || { echo "platform is ${plat:-none}, expected $want_platform"; return 1; }
    if [[ -n $max_minos ]]; then
        [[ -n $minos ]] || { echo "no minimum OS version in $f"; return 1; }
        mac_version_le "$minos" "$max_minos" || { echo "minos $minos is newer than $max_minos, so older macOS cannot run it"; return 1; }
    fi
}

# mac_codesign_verify <file>: strict signature check. Apple Silicon refuses unsigned code, so there a file
# must be signed. On Intel the linker leaves programs unsigned and macOS runs them, so an unsigned file is
# accepted there and a signed one must still verify.
mac_codesign_verify() {
    if [[ $mac_arch != arm64 ]] && codesign -d "$1" 2>&1 | grep -q "not signed at all"; then
        return 0
    fi
    codesign --verify --strict "$@"
}

# mac_signed_ok <file>: the signature is valid and strict (what Apple Silicon checks at launch).
mac_signed_ok() {
    mac_codesign_verify "$1" 2>&1 && codesign -dv "$1" 2>&1 | sed -n '/^Signature\|^Identifier\|^CodeDirectory/p'
}

# mac_toolchain_binary_ok <file> <max-minos or empty>: one program of the toolchain.
mac_toolchain_binary_ok() {
    mac_macho_ok "$1" "$mac_arch" "MACOS|MACOS-LEGACY" "$2" || return 1
    mac_signed_ok "$1" || return 1
    if command -v check_macho_relocatable > /dev/null; then
        check_macho_relocatable "$1" || return 1
    fi
}

# mac_dylibs_signed <dir>...: every dylib in the directories has a valid signature, and there is one.
mac_dylibs_signed() {
    local n=0 f dir
    for dir in "$@"; do
        for f in "$dir"/*.dylib; do
            [[ -f $f ]] || continue
            mac_codesign_verify "$f" || { echo "bad signature: $f"; return 1; }
            n=$((n + 1))
        done
    done
    echo "$n dylibs verified"
    [[ $n -gt 0 ]]
}

# mac_programs_signed <bin dir>: every Mach-O program in it has a valid signature.
mac_programs_signed() {
    local f n=0
    for f in "$1"/*; do
        case $(file -b "$f") in
            *Mach-O*)
                mac_codesign_verify "$f" || { echo "bad signature: $f"; return 1; }
                n=$((n + 1))
                ;;
        esac
    done
    echo "$n programs verified"
    [[ $n -gt 0 ]]
}

# mac_copy_toolchain <toolchain> <copy> <host>: copy what a build of the host needs: bin, lib with
# the rustlib of the host, and the top-level files. The standard libraries of the other targets and
# the documentation are left out, they are large and not what is being proven.
mac_copy_toolchain() {
    local src=$1 dst=$2 host=$3 entry base
    mkdir -p "$dst/lib/rustlib" || return 1
    cp -a "$src/bin" "$dst/bin" || return 1
    for entry in "$src"/*; do
        base=$(basename "$entry")
        case $base in
            bin | lib | share) ;;
            *) cp -a "$entry" "$dst/" || return 1 ;;
        esac
    done
    for entry in "$src"/lib/*; do
        [[ $(basename "$entry") == rustlib ]] || cp -a "$entry" "$dst/lib/" || return 1
    done
    for entry in "$src"/lib/rustlib/*; do
        base=$(basename "$entry")
        if [[ -d $entry && $base == *-*-* && $base != "$host" ]]; then
            continue
        fi
        cp -a "$entry" "$dst/lib/rustlib/" || return 1
    done
}

mac_probe_package() {
    # A package with every crate type. The library uses the standard library, so a std that does not
    # fit the target fails to build rather than links by luck.
    local dir=$1
    mkdir -p "$dir/src"
    cat > "$dir/Cargo.toml" << 'EOF'
[package]
name = "applepkg"
version = "0.1.0"
edition = "2024"

[lib]
crate-type = ["lib", "staticlib", "cdylib"]

[[bin]]
name = "applebin"
path = "src/main.rs"

[profile.release]
strip = "symbols"
opt-level = 2
EOF
    cat > "$dir/src/lib.rs" << 'EOF'
use std::collections::BTreeMap;

#[unsafe(no_mangle)]
pub extern "C" fn apple_add(a: i32, b: i32) -> i32 {
    a + b
}

pub fn describe() -> String {
    let mut map = BTreeMap::new();
    map.insert("os", std::env::consts::OS);
    map.insert("arch", std::env::consts::ARCH);
    format!("{map:?}")
}
EOF
    cat > "$dir/src/main.rs" << 'EOF'
fn main() {
    println!(
        "apple-ok {} {}",
        applepkg::describe(),
        applepkg::apple_add(40, 2)
    );
}
EOF
}

# mac_build_target <target> <extra args...>: frust build of the probe package, results in
# $mac_target_dir/<target>/<profile>.
mac_build_target() {
    local target=$1
    shift
    cd "$mac_pkg" && "$bin/frust" build --offline --target "$target" --target-dir "$mac_target_dir" "$@"
}

# The platform each bundled Apple target must produce, as an egrep alternation. x86_64-apple-ios is
# the simulator for Intel and is accepted with a legacy IOS command too, but never as macOS.
mac_expected_platform() {
    case $1 in
        aarch64-apple-ios) echo 'IOS' ;;
        aarch64-apple-ios-sim) echo 'IOSSIMULATOR' ;;
        x86_64-apple-ios) echo 'IOSSIMULATOR|IOS-LEGACY' ;;
        *-apple-ios-macabi) echo 'MACCATALYST' ;;
        *-apple-darwin) echo 'MACOS|MACOS-LEGACY' ;;
    esac
}

mac_target_arch() {
    case $1 in
        aarch64-*) echo arm64 ;;
        x86_64-*) echo x86_64 ;;
    esac
}

# The SDK that links a target. Mac Catalyst binaries link against the macOS SDK.
mac_target_sdk() {
    case $1 in
        aarch64-apple-ios) echo iphoneos ;;
        *-apple-ios-sim | x86_64-apple-ios) echo iphonesimulator ;;
        *) echo macosx ;;
    esac
}

mac_suite() {
    local f name target arch plat sdk moved debug_dir
    local mach_tools="rustc rustdoc cargo cargo-clippy clippy-driver rustfmt cargo-fmt rust-analyzer fcargo-ui frust-install"
    # The programs bootstrap built with MACOSX_DEPLOYMENT_TARGET set. The rest (fcargo-ui and
    # frust-install are built by the release job, rust-analyzer is an official binary) keep the
    # default of the Rust that built them, so they are checked for architecture, signature and
    # dependencies only.
    local bootstrap_tools=" rustc rustdoc cargo cargo-clippy clippy-driver rustfmt cargo-fmt "

    section "macOS: Mach-O files of the toolchain ($host_triple, deployment target $mac_deployment)"
    if ! have otool || ! have lipo || ! have codesign; then
        mac_unavailable "macOS tools otool, lipo and codesign" "one of them is not on this host"
        return 0
    fi
    for name in $mach_tools; do
        f=$bin/$name
        [[ -f $f ]] || continue
        case $bootstrap_tools in
            *" $name "*) check "$name is a signed, relocatable $mac_arch Mach-O for macOS $mac_deployment or older" mac_toolchain_binary_ok "$f" "$mac_deployment" ;;
            *) check "$name is a signed, relocatable $mac_arch Mach-O" mac_toolchain_binary_ok "$f" "" ;;
        esac
    done
    for name in clippy-driver rustfmt; do
        check_match "$name links the compiler's librustc_driver by @rpath" '@rpath/librustc_driver-[0-9a-f]*\.dylib' otool -L "$bin/$name"
        check_match "$name has the rpath @loader_path/../lib" '@loader_path/\.\./lib' otool -l "$bin/$name"
    done
    check_true "lib has the librustc_driver dylib the tools link" "no $tc/lib/librustc_driver-*.dylib" \
        sh -c "ls '$tc'/lib/librustc_driver-*.dylib"
    # Hard links to one signed file: the signature is in the file, so every name has it.
    for name in "cargo fcargo" "cargo frust" "cargo fclippy" "rustc frustc" "rustdoc frustdoc" "rustfmt frustfmt" "frust-install frustup"; do
        # shellcheck disable=SC2086
        set -- $name
        [[ -e $bin/$2 ]] || continue
        check_true "bin/$2 is a hard link of bin/$1" "different files: $(ls -li "$bin/$1" "$bin/$2" 2>&1 | tr '\n' ' ')" test "$bin/$1" -ef "$bin/$2"
        check "bin/$2 has a valid signature" mac_signed_ok "$bin/$2"
    done
    # Signed files in lib too: the compiler's dylibs and the standard library of the host.
    check "every dylib of lib and lib/rustlib/$host_triple/lib has a valid signature" \
        mac_dylibs_signed "$tc/lib" "$tc/lib/rustlib/$host_triple/lib"

    section "macOS: the toolchain still runs when copied to a path with spaces and non-ASCII characters"
    moved="$work/moved copy é/frust"
    mkdir -p "$(dirname "$moved")"
    if check "copy the toolchain (bin, lib, the rustlib of $host_triple) with cp -a" mac_copy_toolchain "$tc" "$moved" "$host_triple"; then
        mac_probe_package "$work/mac-moved-pkg"
        for name in rustfmt clippy-driver cargo-clippy cargo-fmt rustc cargo fclippy frustfmt; do
            check_match "moved copy: $name --version" '[A-Za-z0-9]' env PATH="$moved/bin:/usr/bin:/bin" "$moved/bin/$name" --version
        done
        check "moved copy: every program keeps a valid signature" mac_programs_signed "$moved/bin"
        check_match "moved copy: rustc --print sysroot is the copy" 'moved copy' env PATH="$moved/bin:/usr/bin:/bin" "$moved/bin/rustc" --print sysroot
        check "moved copy: fclippy checks a package" in_dir "$work/mac-moved-pkg" \
            env PATH="$moved/bin:/usr/bin:/bin" "$moved/bin/fclippy" --offline --target-dir "$work/mac-moved-target"
        check "moved copy: frust fmt --check a formatted package" in_dir "$work/mac-moved-pkg" \
            env PATH="$moved/bin:/usr/bin:/bin" "$moved/bin/frust" fmt --check
        check_match "moved copy: frust build and run" 'apple-ok .*42' sh -c \
            "cd '$work/mac-moved-pkg' && env PATH='$moved/bin:/usr/bin:/bin' '$moved/bin/frust' run --offline --target-dir '$work/mac-moved-target'"
        rm -rf "$work/moved copy é" "$work/mac-moved-target" "$work/mac-moved-pkg"
    fi

    section "macOS: Apple targets, built with frust (lib, staticlib, cdylib and bin)"
    mac_pkg=$work/mac-pkg
    mac_target_dir=$work/mac-target
    mac_probe_package "$mac_pkg"
    # fmt of the generated sources keeps the formatting checks above and below honest.
    check "frustfmt formats the probe package" sh -c "cd '$mac_pkg' && '$bin/frust' fmt"

    local apple_targets="$host_triple"
    for target in $disk_targets; do
        case $target in
            *-apple-*) contains_word "$target" $apple_targets || apple_targets="$apple_targets $target" ;;
        esac
    done
    for target in $apple_targets; do
        sdk=$(mac_target_sdk "$target")
        arch=$(mac_target_arch "$target")
        plat=$(mac_expected_platform "$target")
        if ! have xcrun; then
            mac_unavailable "frust build --target $target" "no xcrun on this host"
            continue
        elif ! xcrun --sdk "$sdk" --show-sdk-path > /dev/null 2>&1; then
            mac_unavailable "frust build --target $target" "the $sdk SDK is not installed (xcrun --sdk $sdk fails)"
            continue
        fi
        if ! check "frust build --target $target (rlib, staticlib, cdylib, bin)" mac_build_target "$target"; then
            continue
        fi
        debug_dir=$mac_target_dir/$target/debug
        for name in libapplepkg.rlib libapplepkg.a libapplepkg.dylib applebin; do
            check_true "$target: $name was produced" "missing $debug_dir/$name" test -s "$debug_dir/$name"
        done
        check "$target: applebin is $arch for $plat" mac_macho_ok "$debug_dir/applebin" "$arch" "$plat"
        check "$target: libapplepkg.dylib is $arch for $plat" mac_macho_ok "$debug_dir/libapplepkg.dylib" "$arch" "$plat"
        check_match "$target: libapplepkg.a is an $arch archive" "$arch" lipo -archs "$debug_dir/libapplepkg.a"
        check_match "$target: libapplepkg.a exports apple_add" 'T _apple_add' sh -c "nm -g '$debug_dir/libapplepkg.a' 2>/dev/null; true"
        check_match "$target: libapplepkg.dylib exports apple_add" 'T _apple_add' sh -c "nm -gU '$debug_dir/libapplepkg.dylib' 2>/dev/null; true"
        # Apple's linker signs ad hoc what it writes for arm64 macOS and the arm64 simulator, so
        # those must verify. Device and Mac Catalyst outputs are signed by Xcode in a real build, so
        # their signature is only shown.
        case $target in
            aarch64-apple-darwin | aarch64-apple-ios-sim)
                check "$target: applebin has a valid signature" mac_signed_ok "$debug_dir/applebin"
                check "$target: libapplepkg.dylib has a valid signature" mac_signed_ok "$debug_dir/libapplepkg.dylib"
                ;;
            aarch64-*) codesign -dv "$debug_dir/applebin" 2>&1 | sed "s|^|      $target applebin: |" ;;
        esac
        check "fclippy --target $target" sh -c \
            "cd '$mac_pkg' && '$bin/fclippy' --offline --target '$target' --target-dir '$mac_target_dir' -- -D warnings"
    done

    section "macOS: a stripped release build keeps its signature and runs"
    if check "frust build --release --target $host_triple (strip = symbols)" mac_build_target "$host_triple" --release; then
        f=$mac_target_dir/$host_triple/release/applebin
        check "release applebin is a signed $mac_arch Mach-O" mac_toolchain_binary_ok "$f" "$mac_deployment"
        check_match "release applebin runs" 'apple-ok .*42' "$f"
    fi
    check_match "the host debug applebin runs" 'apple-ok .*42' "$mac_target_dir/$host_triple/debug/applebin"
    rm -rf "$mac_target_dir"
}

mac_suite
