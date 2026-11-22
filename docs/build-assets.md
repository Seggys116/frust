# Build assets

The shipped Linux x86-64 toolchain is built by `support/optimize-linux-v2.sh` in stored-profile mode. That build needs three files that take hours to produce and that CI does not rebuild: a bundle of LLVM build tools and two bundles of PGO and BOLT profiles, one per CPU variant. The shipped macOS toolchains are built by `.github/scripts/build-toolchain-pgo.sh` in stored-profile mode and need one more file each: the rustc PGO profile of their target. They are assets of a GitHub release, and `support/build-assets.lock` lists them with their digests. The release workflow and the toolchains workflow download them with `support/fetch-build-assets.sh` and refuse anything that does not match the lock.

## The assets

The names follow the pins in `support/upstream.lock`, so a new LLVM or a new Rust commit asks for new files and a stale lock fails with an unknown asset instead of building with the wrong ones.

| Asset | Name | Used for |
| --- | --- | --- |
| Build tools | `frust-build-tools-<llvm_version>-linux-x86_64.tar.zst` (`frust-build-tools-23.1.5-linux-x86_64.tar.zst`) | `clang`, `clang++`, `ld.lld`, `llvm-ar`, `llvm-ranlib`, `llvm-profdata`, `llvm-bolt`, `merge-fdata` and the profile and BOLT runtimes of the pinned LLVM. Compiles the C and C++ parts of rustc, merges and applies profiles, and links. |
| Baseline profiles | `frust-pgo-profiles-<rust_commit first 9>-x86-64.tar.zst` (`frust-pgo-profiles-0ef1bc386-x86-64.tar.zst`) | The PGO profiles for rustc, rustdoc and clippy and the BOLT profile of the rustc driver, for the baseline build. |
| v3 profiles | `frust-pgo-profiles-<rust_commit first 9>-x86-64-v3.tar.zst` (`frust-pgo-profiles-0ef1bc386-x86-64-v3.tar.zst`) | The same four profiles, collected for the build whose compiler and tools target x86-64-v3 (`FRUST_COMPILER_TARGET_CPU=x86-64-v3`). |
| macOS profiles | `frust-pgo-profiles-<rust_commit first 9>-<target>.tar.zst` for `aarch64-apple-darwin` and `x86_64-apple-darwin` (`frust-pgo-profiles-0ef1bc386-aarch64-apple-darwin.tar.zst`) | The rustc PGO profile of the macOS toolchain of that target, collected natively on a runner of its architecture. |

A profile only fits the code it was collected from. A target CPU changes the code that is compiled, so the v3 build has its own profiles and cannot reuse the baseline ones.

Each archive is a `tar.zst` with its files at the root, no wrapping directory:

- Build tools: the install prefix of `support/tools/build-llvm-tools.sh`, that is `bin/`, `lib/` (with `lib/libbolt_rt_instr.a` and the clang resource directory) and `frust-llvm-tools.stamp`.
- Profiles: `rustc-pgo.profdata`, `rustdoc-pgo.profdata`, `clippy-pgo.profdata`, `rustc-bolt.profdata` and a `SHA256SUMS` that `optimize-linux-v2.sh` checks before it uses them. A `MANIFEST` describing the run that produced them is kept alongside.
- macOS profiles: `rustc-pgo.profdata`, a `SHA256SUMS` and a `MANIFEST` in the format of `support/tools/pgo-rustc-bundle.py`.

## How they are produced

1. Build tools. Prepare a checkout of the pinned sources with `support/tools/prepare-rust-checkout.sh DEST`, then run `support/tools/build-llvm-tools.sh DEST/src/llvm-project PREFIX`. It builds the tools from the pinned LLVM commit and stamps the result. Build them on the oldest distribution CI uses (the ubuntu-24.04 runner image), because the tools run there and link against its glibc and libstdc++. Pack `PREFIX` as described above.
2. Baseline profiles. Run `support/optimize-linux-v2.sh` without `FRUST_STORED_PROFILES`, with `FRUST_PROFILE_EXPORT=DIR`. It runs opt-dist: PGO training, a PGO build, BOLT training and the dist step, and exports the four `.profdata` files, `SHA256SUMS` and `MANIFEST` to `DIR`. Pack `DIR`.
3. v3 profiles. Repeat step 2 with `FRUST_COMPILER_TARGET_CPU=x86-64-v3` set, so that the training builds and the exported profiles are those of the v3 compiler. To refresh only the BOLT profile of an existing v3 build, `support/tools/bolt-train-driver.sh` gathers one against that build's pre-BOLT driver (`WORK/rustc-driver-prebolt.so`). Whatever the route, the directory that is packed holds the four `.profdata` files and a `SHA256SUMS` made with `sha256sum -- *.profdata > SHA256SUMS`.

`optimize-linux-v2.sh` documents its modes and variables in its header, and the tools in `support/tools` document their arguments in theirs.

4. macOS profiles. Profiles are only valid on the machine that collected them, so they are made in CI on native runners and not on a Linux host. Dispatch the macOS profiles workflow, which runs `.github/scripts/build-toolchain-pgo.sh generate` once per target (about 45 minutes on Apple Silicon and 3 hours on Intel, inside the 355 minute job limit):

   ```sh
   gh workflow run macos-profiles.yml --ref main
   ```

   The inputs are `targets` (`both`, `aarch64-apple-darwin` or `x86_64-apple-darwin`), `verify` (default true), `cargo-profile` (default false, see below), `train-budget-minutes` (default 80) and `release-tag` (the release named in the upload commands, default `build-assets-1`). Each job builds the instrumented stage-2 `rustc`, checks that it starts, trains it on the rustc-perf crates of `support/patches/build` (upstream's `RUSTC_PGO_CRATES` and the application-style crates, never a measured cohort), merges the raw profiles as they appear, and with `verify` builds the profile-use compiler once to prove that the profile fits it. The bundle is made only when every gate of `support/tools/pgo-rustc-evidence.py` passed. It is the artifact `pgo-profiles-<target>`, a `frust-pgo-profiles-<commit>-<target>.tar.zst` kept for 30 days. The run summary lists the training evidence, the size and digest of each bundle and the publishing commands below. The workflow publishes nothing.

   Publishing is a maintainer's step, from a checkout of the repository, once for each target (or once for both files):

   ```sh
   gh run download RUN_ID -n pgo-profiles-aarch64-apple-darwin -n pgo-profiles-x86_64-apple-darwin -D macos-profiles
   gh release upload build-assets-1 macos-profiles/*/frust-pgo-profiles-*.tar.zst
   bash support/tools/lock-build-asset.sh build-assets-1 macos-profiles/*/frust-pgo-profiles-*.tar.zst
   bash support/fetch-build-assets.sh "$(mktemp -d)" frust-pgo-profiles-0ef1bc386-aarch64-apple-darwin.tar.zst frust-pgo-profiles-0ef1bc386-x86_64-apple-darwin.tar.zst
   ```

   `gh release upload` adds the files to the existing release without touching the other assets (use a new release tag and `--latest=false` as described under the lock when a new release is wanted). `lock-build-asset.sh` writes the lock lines, and the last command downloads the files again and checks them against the lock. Commit the lock afterwards.

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

For the macOS fRust toolchains it unpacks the profile bundle of the target and runs `.github/scripts/build-toolchain-pgo.sh stored` with it. A lock without the bundle stops the build with a message instead of building a toolchain without PGO; `FRUST_RECIPE=plain` builds the plain recipe on purpose.

## Stored macOS profiles

A profile fits only the code it was collected from. `stored` mode runs `support/tools/pgo-rustc-bundle.py check` before it fetches or builds anything else, and refuses a bundle whose `MANIFEST` and `SHA256SUMS` do not match the files, that was made for another Rust commit, LLVM version, target, macOS machine type or deployment target, that was made with other compiler build settings (the hash of the setting lines of `support/bootstrap.toml`, `bootstrap-common.toml` and `bootstrap-macos.toml`, which ignores comments and the job count), whose version of the format is unknown, or whose profile is not an IR-level profile with at least 20000 functions, as the `llvm-profdata` of the official 1.100.0 `llvm-tools` reads it (it must report the pinned LLVM version). A change to those settings, to the pinned Rust commit or to the LLVM pin therefore needs new profiles, and the stale bundle fails in about a minute.

### Cargo profile (experimental, off by default)

With `cargo-profile` set (`FRUST_PGO_CARGO=1` for the script), `generate` also builds an instrumented Cargo (`pgo.cargo.generate`), checks that it starts, and trains it with `support/tools/pgo-cargo-train.py` on build scenarios of rustc-perf crates (`serde-1.0.219`, `clap_derive-4.5.32`, `html5ever-0.31.0`, `hyper-1.6.0`, `eza-0.21.2`; override with `FRUST_PGO_CARGO_CRATES`; budget `FRUST_PGO_CARGO_BUDGET_MINUTES`, default 30). Each crate runs in fCargo mode, dev and release, through a cold build, no-op rebuilds, a leaf edit, a clean rebuild served by the unit cache, `metadata`, `tree` and `check`. The merged profile is the bundle's `cargo-pgo.profdata`, and a `stored` build that finds it uses it as `pgo.cargo.use` and checks the commands and the finished `cargo` binary like the compiler's. A bundle without it builds Cargo without PGO.

Measured on Linux x86-64 only, outside CI, as a proxy: the same patched Cargo built with thin LTO and one codegen unit, trained on the five crates above, then compared with the unprofiled build and with official Cargo 1.100.0 on warm no-op rebuilds of the ripgrep and regex cohorts (release profile, `-j 16`, official `rustc` for all three, 25 and 30 alternating pairs, user plus system CPU). The unprofiled fRust Cargo used 91.6% (ripgrep) and 83.9% (regex) of official CPU, the profiled one 84.0% and 72.6%. That is 8% and 13% less CPU than the unprofiled build. Nothing was measured on macOS, and the macOS pipeline for this profile has not run, so it stays opt-in until a run has produced the bundle and a Benchmark run has measured it.

A changed fRust compiler patch does not refuse the bundle, because it touches few functions. The check lists the patches that differ from the ones in the `MANIFEST`, and the profile-use build then counts the functions whose control flow no longer matches the profile in LLVM's own diagnostics (`-pgo-warn-missing-function` under `-vv`). `support/tools/pgo-rustc-evidence.py` fails the build when more than 5% of the profiled functions mismatch or more than 50% have no profile data, when fewer than 50 compiler commands carry `-Cprofile-use=<the stored profile>`, or when the finished driver library still has profile counter sections. The run summary and the `pgo-evidence-frust-<target>` artifact record the result. Regenerate the profiles after a compiler patch change that moves the mismatch count towards the limit.

`support/tools/test_pgo_rustc_bundle.py` tests the checks with a stand-in `llvm-profdata`; CI runs it.

## Official components

The target libraries, the HTML documentation and the library sources (`rust-src`) in an archive are not built here. They are the official Rust 1.100.0 packages, pinned in `support/official-components.lock` ([how it works](how-it-works.md#what-the-toolchain-contains) explains why they fit). Each line of that file is `bundle component target sha256 url`, with the bundles `linux`, `macos-aarch64` and `macos-x86_64`, and a header that names the release, its date and the sha-256 of the channel manifest the pins were taken from.

- `support/tools/lock-official-components.py CHANNEL.toml MANIFEST_SHA256` writes the lock from an official channel manifest.
- `support/tools/check-official-components-lock.py` checks the lock's shape and that every bundle has the entries the release promises. Without `--offline` it also downloads the channel manifest and compares every pin with the hash the manifest gives.
- `support/fetch-official-components.sh BUNDLE TREE` downloads the packages of one bundle, checks each sha-256 before unpacking, and installs them into a toolchain tree that already holds the fRust host standard library. It never overwrites an existing path, so the host library stays fRust's. `FRUST_OFFICIAL_CACHE=DIR` keeps the downloads between runs.

A new Rust release asks for a new lock: regenerate it from the new channel manifest and run the checker before committing. A stale lock fails the check because its release no longer matches `support/upstream.lock`.
