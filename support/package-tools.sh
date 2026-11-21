#!/bin/bash
# Functions for support/package-toolchain.sh that put clippy and rustfmt into the toolchain tree and
# prove that the tree runs from wherever it is unpacked. Sourced, not run.
#
# clippy and rustfmt link the compiler's own crates (rustc_private), so they are built here, by
# bootstrap, against the exact compiler of the tree: cargo-clippy, clippy-driver, rustfmt and
# cargo-fmt. Bootstrap links each of them with an rpath of @loader_path/../lib on macOS and
# $ORIGIN/../lib elsewhere, which is what makes the tree relocatable. These functions check that this
# really is what came out, because a tool that only runs on the build machine would pass any
# smoke test done there.
#
# macOS notes, from the checks below:
#   - Apple Silicon only runs signed code. Apple's linker ad-hoc signs every binary it writes, and a
#     signature is part of the file, so copying, hard-linking and tar extraction keep it. Rewriting a
#     binary (install_name_tool, strip, writing into the file in place) breaks it. Nothing here
#     does, and sign_unsigned_macho signs, before any hard link is made, whatever came out unsigned.
#   - Hard links of one signed binary (cargo, fcargo, frust, fclippy; rustc, frustc; rustfmt,
#     frustfmt) are fine: they are one file. The tool looks at the name it was started by, not at
#     the file.

# The programs of the clippy and rustfmt packages, and the names that link to them.
FRUST_PACKAGED_TOOLS=(cargo-clippy clippy-driver rustfmt cargo-fmt)

# Print the clean environment that the packaged tools are run in: no library search paths and no
# toolchain variables of the machine that builds them, so only the rpath of the binaries can find
# the libraries. $1 is the tree.
frust_clean_env() {
    printf '%s\n' "env" "-i" "HOME=${TMPDIR:-/tmp}" "PATH=$1/bin:/usr/bin:/bin"
}

# Whether $1 is a Mach-O file (an executable, a dylib or a bundle). Not for fat files of other formats.
is_macho() {
    case $(file -b -- "$1") in
        *Mach-O*) return 0 ;;
        *) return 1 ;;
    esac
}

# Put the four tools in $2/bin. Bootstrap installs them in <stage2>/bin, which package-toolchain.sh
# has copied into the tree already. When it did not (an older bootstrap leaves them only in
# stage2-tools-bin) they come from there. Fails when a tool is nowhere to be found.
# Arguments: <host-build-dir> <tree>
install_clippy_rustfmt() {
    local host_dir=$1 tree=$2 tool built
    for tool in "${FRUST_PACKAGED_TOOLS[@]}"; do
        if [[ ! -f $tree/bin/$tool ]]; then
            built=$host_dir/stage2-tools-bin/$tool
            if [[ ! -f $built ]]; then
                echo "$tool was not built: it is neither in stage2/bin nor in $built." >&2
                echo "The build must run x.py with src/tools/clippy and src/tools/rustfmt." >&2
                return 1
            fi
            install -m 755 "$built" "$tree/bin/$tool"
        fi
        [[ -x $tree/bin/$tool ]] || { echo "$tree/bin/$tool is not executable" >&2; return 1; }
    done
}

# Put rust-analyzer (bin/) and its proc-macro server (libexec/) in $2. Bootstrap builds both with
# the compiler of the tree, which is why they are not the official binaries: those link the
# official build's librustc_driver, which this tree does not have. The server goes to
# <sysroot>/libexec/rust-analyzer-proc-macro-srv, the first place rust-analyzer looks for it (then
# lib/), found from the sysroot that rustc reports. Bootstrap installs the server there itself;
# when it did not, both come from stage2-tools-bin. Fails when one is nowhere to be found.
# Arguments: <host-build-dir> <tree>
install_rust_analyzer() {
    local host_dir=$1 tree=$2 rel name built
    for rel in bin/rust-analyzer libexec/rust-analyzer-proc-macro-srv; do
        name=${rel##*/}
        if [[ ! -f $tree/$rel ]]; then
            built=$host_dir/stage2-tools-bin/$name
            if [[ ! -f $built ]]; then
                echo "$name was not built: it is neither in the tree ($rel) nor in $built." >&2
                echo "The build must run x.py with src/tools/rust-analyzer." >&2
                return 1
            fi
            mkdir -p "$tree/${rel%/*}"
            install -m 755 "$built" "$tree/$rel"
        fi
        [[ -x $tree/$rel ]] || { echo "$tree/$rel is not executable" >&2; return 1; }
    done
}

# Ad-hoc sign every Mach-O file of the tree whose signature is missing or invalid, on Apple Silicon
# only (Intel Macs do not need a signature). Signing writes a new file, so this has to run before
# any hard link to one of the files is made, and it does nothing for a file that is already valid,
# which is every file that Apple's linker wrote.
# Arguments: <tree> <host-triple>
sign_unsigned_macho() {
    local tree=$1 host=$2 file signed=0
    [[ $(uname -s) == Darwin && $host == aarch64-apple-darwin ]] || return 0
    command -v codesign > /dev/null || { echo "codesign is not on PATH" >&2; return 1; }
    while IFS= read -r -d '' file; do
        is_macho "$file" || continue
        if ! codesign --verify --strict "$file" 2> /dev/null; then
            echo "signing $file (ad hoc), its signature was missing or invalid"
            codesign --force --sign - "$file"
            signed=$((signed + 1))
        fi
        codesign --verify --strict "$file" || { echo "$file has no valid signature" >&2; return 1; }
    done < <(find "$tree" -type f \( -name '*.dylib' -o -perm -u+x \) -print0)
    echo "code signatures: all Mach-O files verified ($signed signed here)"
}

# Print the LC_RPATH entries of a Mach-O file, one per line.
macho_rpaths() {
    otool -l "$1" | awk '
        $1 == "cmd" && $2 == "LC_RPATH" { want = 1; next }
        want && $1 == "path" { print $2; want = 0 }'
}

# Print the dependent libraries of a Mach-O file (install names), one per line.
macho_deps() {
    otool -L "$1" | awk 'NR > 1 { print $1 }'
}

# Check that a Mach-O executable of the tree is relocatable. Every dependency is a system library
# (/usr/lib, /System), @rpath, @loader_path or @executable_path, never a path of the build machine,
# and every @rpath dependency exists in a directory that one of the file's rpaths names, resolved
# beside the file. Argument: <executable>
check_macho_relocatable() {
    local file=$1 dir rpath dep name found resolved bad=0
    local rpaths=()
    dir=$(dirname "$file")
    while IFS= read -r rpath; do
        [[ -n $rpath ]] && rpaths+=("$rpath")
    done < <(macho_rpaths "$file")
    while IFS= read -r dep; do
        case $dep in
            /usr/lib/* | /System/*) ;;
            @loader_path/* | @executable_path/*)
                resolved=${dep/#@loader_path/$dir}
                resolved=${resolved/#@executable_path/$dir}
                [[ -e $resolved ]] || { echo "$file: dependency $dep does not exist" >&2; bad=1; }
                ;;
            @rpath/*)
                name=${dep#@rpath/}
                found=0
                # ${rpaths[@]+...}: an empty array is an unbound variable for the bash 3.2 of macOS.
                for rpath in ${rpaths[@]+"${rpaths[@]}"}; do
                    resolved=${rpath/#@loader_path/$dir}
                    resolved=${resolved/#@executable_path/$dir}
                    if [[ -e $resolved/$name ]]; then found=1; break; fi
                done
                [[ $found == 1 ]] || { echo "$file: $dep is not in any of its rpaths (${rpaths[*]:-none})" >&2; bad=1; }
                ;;
            *) echo "$file depends on $dep, a path of the build machine" >&2; bad=1 ;;
        esac
    done < <(macho_deps "$file")
    [[ $bad == 0 ]] || return 1
}

# The same check on Linux: RUNPATH or RPATH holds $ORIGIN/../lib and no dependency has a path.
# Needs readelf, and is skipped with a notice when there is none.
check_elf_relocatable() {
    local file=$1 dynamic
    command -v readelf > /dev/null || { echo "readelf is not on PATH, skipping the rpath check of $file"; return 0; }
    dynamic=$(readelf -d "$file" 2> /dev/null || true)
    case $dynamic in
        *'$ORIGIN/../lib'*) ;;
        *) echo "$file has no \$ORIGIN/../lib in its RUNPATH or RPATH" >&2; return 1 ;;
    esac
}

# Hard links for the command names that stand for another program, made only where the name is not
# taken: fclippy and frust run Cargo (which picks its entry by the name it was started by), and
# frustfmt is rustfmt. Hard links, so every name is one signed file on macOS. Argument: <tree>
link_tool_names() {
    local tree=$1 pair
    for pair in cargo:frust cargo:fclippy rustfmt:frustfmt; do
        [[ -e $tree/bin/${pair#*:} ]] || ln "$tree/bin/${pair%%:*}" "$tree/bin/${pair#*:}"
    done
}

# Prove the tools of the tree: they exist, are one file per name where they should be, run with a
# clean environment from the place they are in, are relocatable, and do their job on a real file.
# Arguments: <tree> <scratch-dir> (the scratch directory must exist and is left empty)
check_clippy_rustfmt() {
    local tree=$1 scratch=$2 tool name out deps rpaths
    local env_cmd=()
    while IFS= read -r name; do env_cmd+=("$name"); done < <(frust_clean_env "$tree")

    for tool in "${FRUST_PACKAGED_TOOLS[@]}"; do
        [[ -x $tree/bin/$tool ]] || { echo "$tree/bin/$tool is missing" >&2; return 1; }
    done
    for name in frust fclippy; do
        [[ $tree/bin/$name -ef $tree/bin/cargo ]] || { echo "bin/$name is not a link to bin/cargo" >&2; return 1; }
    done
    [[ $tree/bin/frustfmt -ef $tree/bin/rustfmt ]] || { echo "bin/frustfmt is not a link to bin/rustfmt" >&2; return 1; }

    if [[ $(uname -s) == Darwin ]]; then
        for tool in rustc rustdoc cargo "${FRUST_PACKAGED_TOOLS[@]}"; do
            check_macho_relocatable "$tree/bin/$tool"
        done
        # The output goes through a variable: grep -q ends its pipe early, which with pipefail
        # would turn a match into a failure.
        for tool in clippy-driver rustfmt; do
            deps=$(macho_deps "$tree/bin/$tool")
            rpaths=$(macho_rpaths "$tree/bin/$tool")
            grep -q '^@rpath/librustc_driver-.*\.dylib$' <<< "$deps" || {
                echo "$tool does not link @rpath/librustc_driver-*.dylib:" >&2
                echo "$deps" >&2
                return 1
            }
            grep -qx '@loader_path/../lib' <<< "$rpaths" || {
                echo "$tool has no @loader_path/../lib rpath, it has: ${rpaths:-none}" >&2
                return 1
            }
        done
    else
        for tool in clippy-driver rustfmt; do
            check_elf_relocatable "$tree/bin/$tool"
        done
    fi

    for tool in rustfmt cargo-fmt clippy-driver cargo-clippy; do
        out=$("${env_cmd[@]}" "$tree/bin/$tool" --version) || { echo "$tool --version failed" >&2; return 1; }
        echo "$tool: $out"
    done
    out=$("${env_cmd[@]}" "$tree/bin/frustfmt" --version) || { echo "frustfmt --version failed" >&2; return 1; }

    # rustfmt rewrites badly formatted code, clippy-driver (given the tree as its sysroot, like
    # cargo-clippy does) reports a clippy lint through the compiler it was built against.
    printf 'fn main(){let x=1;let y=x.clone();println!("{}",y);}\n' > "$scratch/lint.rs"
    "${env_cmd[@]}" "$tree/bin/rustfmt" --edition 2021 "$scratch/lint.rs"
    grep -q 'let x = 1;' "$scratch/lint.rs" || { echo "rustfmt did not format the test file" >&2; return 1; }
    out=$("${env_cmd[@]}" "SYSROOT=$tree" "$tree/bin/clippy-driver" --sysroot "$tree" --edition 2021 \
        --crate-type bin --emit=metadata --out-dir "$scratch" -W clippy::clone_on_copy "$scratch/lint.rs" 2>&1 || true)
    case $out in
        *clone_on_copy*) echo "clippy-driver reported clippy::clone_on_copy through the packaged compiler" ;;
        *) printf 'clippy-driver did not report clippy::clone_on_copy:\n%s\n' "$out" >&2; return 1 ;;
    esac
}

# Prove rust-analyzer and its proc-macro server: they exist, are relocatable, run from the place
# they are in with a clean environment, and the server really expands a derive macro that was
# built by the compiler of the tree (`rust-analyzer diagnostics` fails on the unresolved method
# when the server is missing or cannot load the macro). Uses the tree's own cargo and rustc.
# Arguments: <tree> <scratch-dir> (the scratch directory must exist and is left with a workspace in it)
check_rust_analyzer() {
    local tree=$1 scratch=$2 tool out deps rpaths
    local env_cmd=()
    while IFS= read -r tool; do env_cmd+=("$tool"); done < <(frust_clean_env "$tree")

    for tool in bin/rust-analyzer libexec/rust-analyzer-proc-macro-srv; do
        [[ -x $tree/$tool ]] || { echo "$tree/$tool is missing" >&2; return 1; }
        if [[ $(uname -s) == Darwin ]]; then
            check_macho_relocatable "$tree/$tool"
            deps=$(macho_deps "$tree/$tool")
            rpaths=$(macho_rpaths "$tree/$tool")
            grep -q '^@rpath/librustc_driver-.*\.dylib$' <<< "$deps" || {
                echo "$tool does not link @rpath/librustc_driver-*.dylib:" >&2
                echo "$deps" >&2
                return 1
            }
            grep -qx '@loader_path/../lib' <<< "$rpaths" || {
                echo "$tool has no @loader_path/../lib rpath, it has: ${rpaths:-none}" >&2
                return 1
            }
        else
            check_elf_relocatable "$tree/$tool"
        fi
        # The server refuses to run unless this variable is set (rust-analyzer sets it for it).
        out=$("${env_cmd[@]}" RUST_ANALYZER_INTERNALS_DO_NOT_USE=1 "$tree/$tool" --version) ||
            { echo "$tool --version failed" >&2; return 1; }
        echo "$tool: $out"
    done

    mkdir -p "$scratch/ra/mac/src" "$scratch/ra/app/src"
    printf '[workspace]\nmembers = ["mac", "app"]\nresolver = "2"\n' > "$scratch/ra/Cargo.toml"
    printf '[package]\nname = "mac"\nversion = "0.1.0"\nedition = "2021"\n[lib]\nproc-macro = true\n' \
        > "$scratch/ra/mac/Cargo.toml"
    cat > "$scratch/ra/mac/src/lib.rs" <<'RUST'
use proc_macro::TokenStream;

#[proc_macro_derive(Hello)]
pub fn hello(input: TokenStream) -> TokenStream {
    let text = input.to_string();
    let name = text.split_whitespace().skip_while(|word| *word != "struct").nth(1).unwrap();
    let name = name.trim_end_matches(';');
    let item = String::from("impl ") + name + " { pub fn hello_generated() -> u32 { 42 } }";
    item.parse().unwrap()
}
RUST
    printf '[package]\nname = "app"\nversion = "0.1.0"\nedition = "2021"\n[dependencies]\nmac = { path = "../mac" }\n' \
        > "$scratch/ra/app/Cargo.toml"
    cat > "$scratch/ra/app/src/main.rs" <<'RUST'
use mac::Hello;

#[derive(Hello)]
struct Foo;

fn main() {
    let value: u32 = Foo::hello_generated();
    let _ = value;
}
RUST
    out=$(cd "$scratch/ra" && "${env_cmd[@]}" "$tree/bin/rust-analyzer" diagnostics . 2>&1) || {
        printf 'rust-analyzer found an error in a workspace whose derive macro the proc-macro server must expand:\n%s\n' "$out" >&2
        return 1
    }
    echo "rust-analyzer expanded a derive macro through libexec/rust-analyzer-proc-macro-srv"
}
