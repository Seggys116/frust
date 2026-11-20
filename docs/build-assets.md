# Build assets

The shipped Linux x86-64 toolchain is built by `support/optimize-linux-v2.sh` in stored-profile mode. That build needs three files that take hours to produce and that CI does not rebuild: a bundle of LLVM build tools and two bundles of PGO and BOLT profiles, one per CPU variant. They are assets of a GitHub release, and `support/build-assets.lock` lists them with their digests. The release workflow and the toolchains workflow download them with `support/fetch-build-assets.sh` and refuse anything that does not match the lock.

## The assets

The names follow the pins in `support/upstream.lock`, so a new LLVM or a new Rust commit asks for new files and a stale lock fails with an unknown asset instead of building with the wrong ones.

| Asset | Name | Used for |
| --- | --- | --- |
| Build tools | `frust-build-tools-<llvm_version>-linux-x86_64.tar.zst` (`frust-build-tools-23.1.5-linux-x86_64.tar.zst`) | `clang`, `clang++`, `ld.lld`, `llvm-ar`, `llvm-ranlib`, `llvm-profdata`, `llvm-bolt`, `merge-fdata` and the profile and BOLT runtimes of the pinned LLVM. Compiles the C and C++ parts of rustc, merges and applies profiles, and links. |
| Baseline profiles | `frust-pgo-profiles-<rust_commit first 9>-x86-64.tar.zst` (`frust-pgo-profiles-0ef1bc386-x86-64.tar.zst`) | The PGO profiles for rustc, rustdoc and clippy and the BOLT profile of the rustc driver, for the baseline build. |
| v3 profiles | `frust-pgo-profiles-<rust_commit first 9>-x86-64-v3.tar.zst` (`frust-pgo-profiles-0ef1bc386-x86-64-v3.tar.zst`) | The same four profiles, collected for the build whose compiler and tools target x86-64-v3 (`FRUST_COMPILER_TARGET_CPU=x86-64-v3`). |

A profile only fits the code it was collected from. A target CPU changes the code that is compiled, so the v3 build has its own profiles and cannot reuse the baseline ones.

Each archive is a `tar.zst` with its files at the root, no wrapping directory:

- Build tools: the install prefix of `support/tools/build-llvm-tools.sh`, that is `bin/`, `lib/` (with `lib/libbolt_rt_instr.a` and the clang resource directory) and `frust-llvm-tools.stamp`.
- Profiles: `rustc-pgo.profdata`, `rustdoc-pgo.profdata`, `clippy-pgo.profdata`, `rustc-bolt.profdata` and a `SHA256SUMS` that `optimize-linux-v2.sh` checks before it uses them. A `MANIFEST` describing the run that produced them is kept alongside.

## How they are produced

1. Build tools. Prepare a checkout of the pinned sources with `support/tools/prepare-rust-checkout.sh DEST`, then run `support/tools/build-llvm-tools.sh DEST/src/llvm-project PREFIX`. It builds the tools from the pinned LLVM commit and stamps the result. Build them on the oldest distribution CI uses (the ubuntu-24.04 runner image), because the tools run there and link against its glibc and libstdc++. Pack `PREFIX` as described above.
2. Baseline profiles. Run `support/optimize-linux-v2.sh` without `FRUST_STORED_PROFILES`, with `FRUST_PROFILE_EXPORT=DIR`. It runs opt-dist: PGO training, a PGO build, BOLT training and the dist step, and exports the four `.profdata` files, `SHA256SUMS` and `MANIFEST` to `DIR`. Pack `DIR`.
3. v3 profiles. Repeat step 2 with `FRUST_COMPILER_TARGET_CPU=x86-64-v3` set, so that the training builds and the exported profiles are those of the v3 compiler. To refresh only the BOLT profile of an existing v3 build, `support/tools/bolt-train-driver.sh` gathers one against that build's pre-BOLT driver (`WORK/rustc-driver-prebolt.so`). Whatever the route, the directory that is packed holds the four `.profdata` files and a `SHA256SUMS` made with `sha256sum -- *.profdata > SHA256SUMS`.

`optimize-linux-v2.sh` documents its modes and variables in its header, and the tools in `support/tools` document their arguments in theirs.

Pack an archive with the files at its root:

```sh
tar -C DIR -cf - . | zstd -19 -T0 -o NAME.tar.zst
```

## The lock

`support/build-assets.lock` is a text file. A comment names the release that holds the assets, and each asset has one line:

```
# release: <tag>
<name> <sha256> <size in bytes> <url>
```

The digest is lowercase hex, the url is `https://` (or `file://` for a local test), and blank lines and other `#` lines are ignored. The file is part of the cache key of the Linux toolchain builds, so changing it rebuilds them.

To refresh it:

1. Produce the assets as above.
2. Create the release that will hold them and upload the files. Make it a normal release that is not marked latest: `frust-install` downloads from `releases/latest`, which must stay the toolchain release. For example `gh release create <tag> --latest=false --title "build assets <tag>" NAME.tar.zst ...`.
3. For each file write its line: `sha256sum NAME.tar.zst`, `stat -c %s NAME.tar.zst` and the download url `https://github.com/Seggys116/frust/releases/download/<tag>/NAME.tar.zst`. Set the `# release:` comment to the tag.
4. Check the lock against the uploaded files before committing it: `bash support/fetch-build-assets.sh "$(mktemp -d)" NAME.tar.zst ...` downloads each file, compares the size and digest and unpacks it.
5. Commit the lock. Do not edit a digest by hand; a mismatch means the asset or the line is wrong.

## Fetching by hand

```sh
bash support/fetch-build-assets.sh DEST NAME...
```

Each asset is verified against the lock and then unpacked into `DEST/<name without .tar.zst>`; the script prints one `<name> <directory>` line per asset on standard output and logs on standard error. A name the lock does not list, a missing lock and a size or digest mismatch are errors, and nothing is unpacked in the last case. An asset already unpacked from the same digest is not downloaded again. `FRUST_BUILD_ASSETS_LOCK` points the script at another lock file. It needs `curl`, `zstd`, `tar`, `awk` and `sha256sum` or `shasum`.

`.github/scripts/build-toolchain.sh` calls it for the Linux x86-64 fRust toolchain, passes the unpacked build tools as the LLVM tools prefix and the unpacked profiles as `FRUST_STORED_PROFILES`, and adds `FRUST_COMPILER_TARGET_CPU=x86-64-v3` for the v3 variant.
