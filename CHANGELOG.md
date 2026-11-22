# Changelog

All notable changes are listed here. The format follows [Keep a Changelog](https://keepachangelog.com/en/1.1.0/).

## [Unreleased]

* 22 November 2026: Cargo start-up costs less on Linux (patch 0030, and a non-PIE Cargo link in `support/optimize-linux-v2.sh`). libcurl and OpenSSL are initialized when the first handle is created and not by a constructor before `main`, `cargo build` skips opening the global cache database when no last-use data is pending, and a command that names a builtin subcommand builds only that subcommand's argument definition. The Linux Cargo is linked as a non-PIE executable, which removes about 80000 start-up relocations. On a local unoptimized build with the 0.6.0 compiler, a warm single-package check of a small workspace used about 68% of 0.6.0 Cargo CPU (the host was heavily loaded, so only the paired ratio means anything) and its peak RSS fell from 26.1 to 20.2 MiB, and the 388-member warm check used 73% of 0.6.0's peak RSS. These are local figures from a build without PGO and BOLT. Benchmark CI has not run them. macOS is unchanged.

* 22 November 2026: Benchmark CI takes the two builds of each scenario back to back with the first toolchain alternating, alternates the no-op builds of the two toolchains, warms both toolchains and quietens the runner before it measures. The report judges each cell with a trimmed mean of paired log ratios, a Yuen and bootstrap interval and a sign test, states win, parity, loss or insufficient with the pairs each needs, and writes `verdict.json` for the release bar. `rounds` defaults to auto, and `NxS` runs shards whose pairs are pooled. One `package` job per platform makes the release archive that the compare jobs install. The statistics have known-value and synthetic-data tests in CI.
* 22 November 2026: the macOS toolchains are built with the compiler compiled by rustc PGO, from a stored profile bundle per target (`frust-pgo-profiles-<commit>-<target>`, a build asset like the Linux profiles) and without training in the release build. A new macOS profiles workflow makes the bundles natively on both runners, and a build refuses a bundle that was made for another Rust commit, LLVM, target, machine or set of compiler build settings, or whose profile no longer fits the code. The native PGO candidate measured 93.3% (Intel) and 95.8% (Apple Silicon) of official CPU for cold builds. See `docs/build-assets.md` and `docs/benchmarks.md`.
* 20 November 2026: the interface-hash cutoff was judged on the full paired Linux cohort and not kept. Build-time gates passed and the runtime aggregate and stripped-size gates failed, because the three application binaries matched stock. No candidate has passed every frozen Linux budget. See `qualification/GATE-REPORT.md` and `docs/benchmarks.md`.
* 20 November 2026: prelink-span reuse (`frust-0ef1bc386-prelinkspan1`) was judged on the same cohort and not kept. The leaf build budgets passed. Disk, the runtime aggregate, and stripped size failed. The three application binaries matched stock. See `qualification/GATE-REPORT.md` and `docs/benchmarks.md`.
* 20 November 2026: an x86-64-v3 codegen preflight on that compiler, with the hotfreq16 flags and opt-in RELR, was not kept. Oxipng's median ratio was 0.94419073. The three-workload runtime geomean was 0.98290005, and rustfmt's median was 1.01459543. Stripped size beat 0.97 only while RELR stayed on. No full judge was run. See `qualification/GATE-REPORT.md` and `docs/benchmarks.md`.
* 20 November 2026: a ripgrep build with the machine outliner and `-C force-unwind-tables=no`, and without RELR, was not kept. Stripped size was 4010848/4024992 (0.99648596). That ratio cannot carry a 0.97 size geomean. Oxipng, rustfmt, and the full judge were not run. See `qualification/GATE-REPORT.md` and `docs/benchmarks.md`.
* 20 November 2026: enabling loop fusion in LLVM's `default<O3>` and `lto<O3>` pipelines left the memchr and ripgrep bitcode unchanged. The switch is not a `rustc` user flag. No stage-2 rebuild and no full judge. See `qualification/GATE-REPORT.md` and `docs/benchmarks.md`.
* 20 November 2026: `-attributor-enable=full` crashed `frustc` with SIGSEGV while optimizing regex-automata 0.4.15. No ripgrep binary was produced. No full judge. See `qualification/GATE-REPORT.md` and `docs/benchmarks.md`.
* 20 November 2026: enabling NewGVN in LLVM's `lto<O3>` pipeline grew the ripgrep pre-LTO module by 9856 bytes and left `Finder::find_impl` unchanged. The memchr module's textual differences are SSA and block names. An earlier final-crate flag sample had already missed the runtime aggregate and stripped size. No stage-2 rebuild and no new full judge. See `qualification/GATE-REPORT.md` and `docs/benchmarks.md`.
* 20 November 2026: `-enable-gvn-sink` is not part of LLVM's `lto<O3>` pipeline. On the frozen memchr module it left `find_impl` at 695 instructions. No stage-2 rebuild and no full judge. See `qualification/GATE-REPORT.md` and `docs/benchmarks.md`.
* 20 November 2026: `-extra-vectorizer-passes` runs inside LLVM's `lto<O3>` pipeline. It shrank the ripgrep pre-LTO module by 432 bytes and left `Finder::find_impl` unchanged. An earlier span1 sample of the same flag had already missed the runtime aggregate (0.99336408 / 0.99959463) and stripped size (0.99993278). This comparison was not judged again. See `qualification/GATE-REPORT.md` and `docs/benchmarks.md`.
* 21 November 2026: the O3 passes missing from the fat-LTO post-link pipeline (`correlated-propagation`, `reassociate`, `libcalls-shrinkwrap`, `adce`, `loop-rotate`, `loop-idiom`, `simple-loop-unswitch`, and `loop-interchange`) were run after `lto<O3>` and not added to rustc. The ripgrep module's instruction ratio was 0.99040271. `find_impl` kept its vector compare. No stage-2 rebuild and no full judge. See `qualification/GATE-REPORT.md` and `docs/benchmarks.md`.
* 21 November 2026: no joint candidate was packaged. The prelink-span judge still fails disk (1.25765371), the runtime aggregate (geomean 1.00478501, upper 1.01877364), and stripped size (1.00000000). hotbudget, the fastest judged full-LTO stack, still has runtime upper 0.98418770 and size geomean 1.03517556. The oxipng `-o 4` hot path remains the libdeflate hash chain. No fresh full judge. See `qualification/GATE-REPORT.md` and `docs/benchmarks.md`.
* 21 November 2026: span-cache relocation plus `.rodata` dedup was preflighted and not compiled in. Removing the measured 26820608 cache bytes from the ripgrep trace leaves disk ratio 1.10520802. Folding every repeated 32-byte `.rodata` window leaves a stripped-size geomean of 0.98749874. No fresh full judge. See `qualification/GATE-REPORT.md` and `docs/benchmarks.md`.
* 21 November 2026: no full-LTO change without RELR was compiled in. Replacing every dynamic relocation with a 4-byte relative pointer would leave a stripped-size geomean of 0.93815212. The measured static-relocation stack reaches 0.93980369 and misses the runtime upper at 0.99106385. No fresh full judge. See `qualification/GATE-REPORT.md` and `docs/benchmarks.md`.
* 21 November 2026: no full-LTO compiler was packaged for the four remaining budgets. None of the 23 saved judges has a runtime upper and a stripped-size geomean both at or under 0.97. The prelink-span compiler passes the leaf budgets at 0.38807512 / 0.38955465 and fails disk, runtime, and stripped size. No fresh full judge. See `qualification/GATE-REPORT.md` and `docs/benchmarks.md`.
* 21 November 2026: forcing MIR inlining on the frozen incremental profile was preflighted and not compiled in. Stripped ripgrep grew to 4030752/4024992 (1.00143106). Thirty pairs had median 0.99684879, and the interquartile range crossed 1. No fresh full judge. See `qualification/GATE-REPORT.md` and `docs/benchmarks.md`.
* 21 November 2026: forcing dataflow const-prop on the frozen incremental profile was preflighted and not compiled in. Stripped ripgrep was 4015920/4024992 (0.99774608). Thirty pairs had median 1.00402123, and the interquartile range crossed 1. The AVX2 `find_impl` body matched the control. No fresh full judge. See `qualification/GATE-REPORT.md` and `docs/benchmarks.md`.
* 21 November 2026: `-Z mir-opt-level=3` on `frust-0ef1bc386-prelinkspan1` was measured and not compiled in. It turns the MIR inliner on and runs limited dataflow const-prop. Leaf passed (geomean 0.39330222, upper 0.39545133). Disk failed at 1.20340838. Runtime aggregate failed at geomean 0.99079798, upper 0.99794438. Stripped size failed because oxipng grew from 1338440 to 1338800. Shipped `frust-gates` exited 1. No compiler was packaged. See `qualification/GATE-REPORT.md` and `docs/benchmarks.md`.
* 21 November 2026: the 630-pair cohort was not started. The MIR opt level 3 runtime geomean is 0.99079798, and the shipped upper-bound tests show that the upper is at least the geomean. Subtracting the 26050560 span-cache bytes from the ripgrep trace leaves disk ratio 1.05533956 and does not change the binaries. No compiler was packaged. See `qualification/GATE-REPORT.md` and `docs/benchmarks.md`.
* 21 November 2026: no new full-LTO compiler was built for a joint preflight of build-time at or under 0.80, a runtime upper at or under 0.97, and every stripped file at or under stock. Shipped `frust-gates` on the hotbudget samples exited 1 and matched `logs/hotbudget-gates.json`: runtime geomean 0.97299293, upper 0.98418770, and ripgrep grew from 4024992 to 4030408. The 630-pair cohort was not started. See `qualification/GATE-REPORT.md` and `docs/benchmarks.md`.
* 21 November 2026: the six backend switches were measured together on `frust-0ef1bc386-prelinkspan1` and not kept. Leaf geomean 0.99252570, disk 1.00071044, runtime upper 0.99910136, and rustfmt grew from 3835192 to 3835544. Shipped `frust-gates` exited 1. The 630-pair cohort was not started. See `qualification/GATE-REPORT.md` and `docs/benchmarks.md`.
* 21 November 2026: the same-candidate four-budget compiler was not built. It would keep an empty `llvm_args` list, move the span caches outside the traced target, keep unwind tables, and leave RELR off. Deleting every `.data.rel.ro` byte of the stock stripped files leaves a size geomean of 0.95941447, which is under 0.97. Those bytes stay, because the program addresses them. The fastest judged runtime geomean, hotbudget at 0.97299293, is already above 0.97. No saved judge passes all four budgets. The 630-pair cohort was not started. See `qualification/GATE-REPORT.md` and `docs/benchmarks.md`.
* 21 November 2026: span-cache relocation plus baked ext-TSP and SelectOptimize (`frust-0ef1bc386-spanout1`) was judged on the full paired Linux cohort and not kept. Edited-release, holdout, cold, no-op, tail, peak memory, disk, and per-workload runtime passed. The runtime aggregate failed at geomean 0.99472611, upper 0.99755978. Stripped size failed at geomean 0.99716103, with every file under stock. See `qualification/GATE-REPORT.md` and `docs/benchmarks.md`.
* 21 November 2026: `-enable-module-inliner` was preflighted on `frust-0ef1bc386-spanout1` and not kept. The flag was passed only on the fRust side, so the span caches and the baked layout switches stayed off. Runtime medians were 0.99549373, 1.00128714, and 1.02205287. The runtime aggregate failed at geomean 1.00621358, upper 1.01338880. Stripped size failed at geomean 1.04715783, and every file grew. No prefix was packaged. See `qualification/GATE-REPORT.md` and `docs/benchmarks.md`.
* 21 November 2026: `-enable-loop-flatten` was preflighted on `frust-0ef1bc386-spanout1` and not kept. The three unstripped binaries matched the control, so stripped size stayed at geomean 1.00000000. The runtime aggregate failed at geomean 0.99840894, upper 1.00569215. No prefix was packaged. See `qualification/GATE-REPORT.md` and `docs/benchmarks.md`.
* 21 November 2026: `-enable-unroll-and-jam` was preflighted on `frust-0ef1bc386-spanout1` and not kept. Stripped sizes stayed at stock, geomean 1.00000000. The runtime aggregate failed at geomean 1.00169953, upper 1.00500825. No prefix was packaged. See `qualification/GATE-REPORT.md` and `docs/benchmarks.md`.
* 21 November 2026: `-enable-partial-inlining` was preflighted on `frust-0ef1bc386-spanout1` and not kept. Rustfmt's paired median was 0.97576234 and its interquartile range stayed under 1. The runtime aggregate still failed at geomean 0.99116818, upper 0.99607085, and every stripped file grew. Size geomean 1.00397687. No prefix was packaged. See `qualification/GATE-REPORT.md` and `docs/benchmarks.md`.
* 21 November 2026: function merging was checked on `frust-0ef1bc386-spanout1` and is already on at opt-level 3. Two identical `#[inline(never)]` functions compile to one symbol. `-Z merge-functions=disabled` keeps both and grows the stripped file from 289344 to 294800 bytes. `-C llvm-args=-enable-merge-functions` is byte-identical when the driver is named `rustc`. No workload preflight was started. See `qualification/GATE-REPORT.md` and `docs/benchmarks.md`.
* 21 November 2026: raising the LICM MemorySSA caps to 10000 was baked into `frust-0ef1bc386-licmcap1` with ext-TSP and SelectOptimize, and `llvm_args` stayed empty. Disk passed at 1.00010149. The runtime aggregate failed at geomean 0.99093255, upper 0.99615678. Stripped size failed at geomean 0.99720509, with every file under stock. The 630-pair cohort was not started. See `qualification/GATE-REPORT.md` and `docs/benchmarks.md`.
* 21 November 2026: `-Z share-generics=on` was tried on one fat-LTO ripgrep build with `frust-0ef1bc386-licmcap1` and not kept. Stripped size grew from the licmcap default 4012576 to 4054736, which is over stock 4024992 (1.00738983). Corpus stdout still matches `118c0e6684d6cc46308cdd44baaa594af0a5cc83d19aa282ef5fcfc48e2e925f`. Oxipng, rustfmt, and the runtime pairs were not started. See `qualification/GATE-REPORT.md` and `docs/benchmarks.md`.
* 21 November 2026: no untried full-LTO change has a path to a runtime upper and a stripped-size geomean both at or under 0.97. Thirty-one saved reports have no runtime upper at or under 0.97. The licmcap product 0.97304357 would have to fall to 0.89800584 before that stack's measured upper gap could reach 0.97. No preflight was started, and the 630-pair cohort was not started. See `qualification/GATE-REPORT.md` and `docs/benchmarks.md`.
* 21 November 2026: post-link BOLT after a fat-LTO link was preflighted and not kept. The rewrite used `--use-old-text --no-huge-pages --align-text=4096 --lite=false --use-gnu-stack`, and ripgrep was trained on the fixed string `epsilon` rather than the timed needle. Stripped sizes grew to 4110536/4024992, 1354288/1338440, and 3949408/3835192. Shipped `frust-gates` exited 1: runtime geomean 1.00346372, upper 1.00621331, size geomean 1.020932. Leaf collection was stopped after the runtime and size misses. The 630-pair cohort was not started. See `qualification/GATE-REPORT.md` and `docs/benchmarks.md`.
* 21 November 2026: `-sink-common-insts` was preflighted on fat LTO, with the licmcap layout flags repeated as user `-C llvm-args`, and not kept. Stripped sizes grew to 4082336/4024992, 1382560/1338440, and 3996104/3835192. Shipped `frust-gates` exited 1: runtime geomean 1.00555891, upper 1.00902087, size geomean 1.02965759. The 630-pair cohort was not started. See `qualification/GATE-REPORT.md` and `docs/benchmarks.md`.
* 21 November 2026: `-enable-knowledge-retention` was preflighted on fat LTO, with the licmcap layout flags repeated as user `-C llvm-args`, and not kept. Every stripped file stayed under stock (4019392/4024992, 1336456/1338440, 3827128/3835192). Shipped `frust-gates` exited 1: runtime geomean 0.99701744, upper 1.00079480, size geomean 0.99834120. The 630-pair cohort was not started. See `qualification/GATE-REPORT.md` and `docs/benchmarks.md`.

## [0.6.1] - 2026-11-22

### Changed

* Cooperative checks prepare compiler commands after acquiring the unit lock and checking whether another process already built the unit. Warm checks replay the recorded diagnostics.
* Cargo downloads that only read already loaded, path or vendored packages use the shared package cache lock. Registry and Git downloads and cache bookkeeping still take the exclusive download lock.
* The unit cache embeds short dep-info and diagnostic text in the manifest, with the same length and digest checks as separate blobs. Existing cache entries still restore.
* Cargo's build invocation markers are empty on Unix. Their filesystem modification time still records when each build started.

### Fixed

* Cooperative checks retry under the build-directory lock if the system runs out of file descriptors while opening a unit lock.

### Benchmarking

* Benchmark CI reports paired intervals for CPU comparisons with official Rust and for resource comparisons with fRust 0.6.0.
* The APFS write trace observes marker writes at the start and end of each window and waits for the device to go quiet. All 24 calibration bursts passed on each macOS runner. Five Intel bursts had writes after the old one-second cutoff. [APFS calibration](https://github.com/Seggys116/frust/actions/runs/45014543999)
* The experimental macOS PGO build completed native training and profile-use rebuilds on both architectures. It uses Rust instrumentation and uploads the merged profiles and build evidence. [Native PGO run](https://github.com/Seggys116/frust/actions/runs/45017675275)
* Wide warm checks used 84.4% to 87.7% of fRust 0.6.0's peak process RSS across the four platforms. Warm proc-macro checks spent less CPU on contended locks, with total CPU intervals spanning parity. Both Macs recorded complete APFS traces and used less occupied space in all four states. Write-volume gains varied by state. [Resource results](docs/benchmarks.md#resource-results-for-the-061-candidate)
* The platform geometric means for clean rebuilds used 43.4% to 61.5% of official Rust 1.100.0 CPU. The Linux no-op means were 89.5% and 90.1%. Cold and leaf-edit aggregate intervals included parity on all four platforms. Targeted regex no-ops used 121.8% of official CPU on Apple Silicon and 105.4% on Intel, establishing a regression on each. The candidate misses the across-platform performance gate. [Build CPU results](docs/benchmarks.md#build-cpu-for-the-061-candidate)

## [0.6.0] - 2026-11-22

### Added

* The unit cache restores unpacked debug objects with dependency outputs, including the default macOS dev profile. The release suite compares restored Darwin objects byte for byte against an uncached build.
* Benchmark CI compares concurrent builds and command startup against the matching fRust 0.5.2 archives. It records paired CPU time, peak RSS, lock waits and hold durations, allocated bytes and write volume on Linux and APFS.
* Benchmark CI adds experimental native rustc PGO jobs on both macOS architectures. The instrumented compiler failed at startup before training in the attempted runs. The release uses the plain macOS recipe. [PGO experiment](docs/benchmarks.md#native-rustc-pgo-candidate-on-macos)

### Changed

* Cooperative checks run on local macOS volumes and can share eligible host proc macros and rlibs. Graphs with build scripts use ordinary Cargo scheduling.
* Cooperative fingerprint probes share one process environment snapshot across the build graph.
* Dependency restores verify the bytes while copying them or after cloning. On cache filesystems without reflinks, an unpacked debug object that matches an rlib member uses that verified byte range.
* Cache writers batch size accounting and use a nonblocking maintenance lock for recounts. The recount timestamp tracks full scans so that active writers cannot postpone a recount indefinitely.
* The legacy-layout launcher skips the profile size scan when there is no cap to enforce. Plain `fclippy` dispatches in the first Cargo process.
* Implicit Cargo parallelism on Linux takes available memory and cgroup v2 headroom into account. Explicit job limits and inherited jobservers still take precedence.

### Performance

* Clean release rebuilds used 43.0% to 63.2% of official Rust 1.100.0 CPU across the four archives. Linux no-op rebuilds used 89.4% to 90.8%. Cold builds and edit rebuilds showed no consistent CPU win, and macOS no-op results were within run-to-run variation. [CPU comparison](docs/benchmarks.md#results-of-the-ci-comparison)
* Compared with 0.5.2, the wide warm check used less peak RSS on all four platforms. The eligible proc-macro fixture went from two build-directory lock waits to zero. Some small warm checks used more CPU. Storage gains were workload-specific, and the paired trials found no general reduction in allocated footprint. Intel macOS write volume remains unresolved because of incomplete traces. [Resource comparisons](docs/benchmarks.md#changes-from-frust-052)

## [0.5.2] - 2026-11-21

### Fixed

* The 0.5.1 archives shipped only `cargo`, `rustc` and `rustdoc` with the standard library of the host. They had no clippy, no rustfmt, no rust-analyzer, no documentation and no other targets, so `fcargo clippy` failed with "cargo-clippy is not installed", and `fcargo fmt` only ran when a rustup `cargo-fmt` happened to be on `PATH`. The toolchain had also never been run the way a user runs it. Every archive now has to pass the validation suite below before a release is published.

### Added

* `cargo-clippy`, `clippy-driver`, `rustfmt` and `cargo-fmt`, built from the same pinned sources as the compiler. On Linux x86-64 clippy is built with the stored PGO profile.
* `rust-analyzer` (with its proc-macro server), built from source with the fRust compiler like clippy and rustfmt, and the `rust-docs` HTML documentation and `rust-src` library sources, taken from the official packages. The official rust-analyzer links the official compiler driver, which an fRust tree does not have, so it cannot be used.
* Target libraries bundled in the archive. Linux x86-64: `x86_64-unknown-linux-musl`, `aarch64-unknown-linux-gnu`, `wasm32-unknown-unknown` and `wasm32-wasip1`, next to the host. macOS on Apple Silicon and on Intel: both `apple-darwin` targets, `aarch64-apple-ios`, `aarch64-apple-ios-sim`, `x86_64-apple-ios`, `aarch64-apple-ios-macabi`, `x86_64-apple-ios-macabi`, `wasm32-unknown-unknown` and `wasm32-wasip1`. These come from the official `rust-std` packages of the same release and commit, pinned by sha-256 in [support/official-components.lock](support/official-components.lock). That is sound because the fRust compiler reports the same `rustc -vV` as official Rust 1.100.0 (commit `0ef1bc386`), so it reads and writes the same crate metadata. Clippy, rustfmt and rust-analyzer link the compiler's internal crates and are built here instead.
* The commands `frust` (the same program as `fcargo`; `frust where` prints the central target directory of the workspace in the current directory), `fclippy`, `frustfmt` and `frustup`. `frustup` is `frust-install` under the name of the tool it replaces.
* `frust-install target add|remove|list` and `frust-install component add|remove|list`. `target` takes any target of the official Rust 1.100.0 channel and `component` takes `rust-docs`, `rust-src` or `llvm-tools`; clippy, rustfmt and rust-analyzer are built into the archive and cannot be added or removed. Packages are fetched through the official channel manifest and verified against its sha-256. `frust-install show` and `frust-install which TOOL` print what is installed. Added targets and components are applied again after `frust-install update`.
* `support/toolchain-suite.sh`, which runs an installed toolchain the way a user does: every command, clippy, rustfmt, rust-analyzer, `frust where`, builds for the bundled targets, and the installer's target and component commands. A release archive has to pass it before it is published.
* [docs/how-it-works.md](docs/how-it-works.md#what-the-toolchain-contains) lists what is built from source and what is taken from the official packages.

## [0.5.1] - 2026-11-21

### Added

* A second Linux x86-64 archive, built for x86-64-v3. `frust-install` selects it automatically on a CPU that supports it and installs the baseline build otherwise. `FRUST_VARIANT=auto`, `baseline` or `v3` overrides the choice, and `update` keeps the installed variant. See [Installer variants](docs/configuration.md#installer-variants).
* Benchmark CI with unit-cache-aware cold, clean rebuild, no-op rebuild and edit rebuild series on Linux and macOS. It records the page-cache state of both toolchains. The first published run is in [docs/benchmarks.md](docs/benchmarks.md#results-of-the-ci-comparison); no CPU numbers are repeated here.

### Changed

* The release toolchains are built with the optimizations that upstream's own dist builds use. On all platforms: one codegen unit, jemalloc, and ThinLTO for the C and C++ parts. On Linux: clang, lld and a static libstdc++. On Linux x86-64 also PGO and BOLT from stored profiles. The profile and tool assets are the `build-assets-1` release, pinned by sha256 in [support/build-assets.lock](support/build-assets.lock); see [docs/build-assets.md](docs/build-assets.md).
* On Linux, `frust-install` rewrites large files with large writes after extraction, so the installed compiler's code is mapped with 2 MiB pages.

### Fixed

* The 0.5.0 toolchains were built without the optimizations listed above and used more CPU than official Rust on cold builds.

## [0.5.0] - 2026-11-20

* Native Linux x86-64 fCargo builds now default to incremental compilation with 16 codegen units for local packages using fat LTO at opt level 3. Explicit Cargo profile and incremental settings take precedence.

Compiler rewrite. fRust is now a Rust 1.100.0 toolchain built from pinned upstream sources, with a patched Cargo (`fcargo`), `frustc`, the unit and build script cache, central targets and `frust-install`. The earlier 0.4.x line is kept on the `legacy-0.4` branch. Toolchains are built for Linux x86-64, macOS on Apple Silicon and macOS on Intel by the release workflow. See `docs/benchmarks.md` and `qualification/CANDIDATE.md`.

### Added

* Started the fRust compiler rewrite and retired the previous build accelerator from the active branch. The entries for 0.3.0 and earlier describe that accelerator.
* `fcargo`, which runs Cargo and sets `RUSTC` to a sibling `frustc` when unset. `FRUST_PROGRESS` selects plain, static or interactive progress, and plain, dumb-terminal, quiet and explicit JSON invocations hand over to Cargo. `FRUST_PROGRESS_COLOR`, `FRUST_BUILD_REPORT` and `FRUST_CARGO` are also read.
* `frust-install`, which installs, updates, stages, rolls back and uninstalls a toolchain. With no command it downloads the latest release for the host from GitHub, verifies the sha-256 and installs it, and `frust-install update` replaces it when the release id changes. It can also install a local archive and sha-256 file. The checksum is verified before extraction and the `current` symlink changes only after the tree is complete. The release workflow builds toolchains for Linux x86-64 and for macOS on Apple Silicon and Intel; the macOS ones are unverified until a release run passes on them.
* `frust-gates`, which recomputes the frozen Linux budgets from raw paired samples.
* Cargo patch for central target directories and a user JSON config file (`config.json` with `version`, `target_root` and `cache_root`), active under `fcargo`. Outputs go to a per-user directory named from the toolchain id and the workspace path. An `artifacts` cache directory with a marker file is created.
* Cargo patch (`0002-frustc-content-fingerprint.patch`) that compares content checksums instead of timestamps when Cargo's compiler path is named `frustc`.
* Eligible `fcargo check` invocations on x86-64 GNU/Linux share verified unit outputs by default. Independent work can run while a peer compiles a shared unit. A stalled check releases its reader locks and retries serially. Set `FRUST_UNIT_JOIN=off` to use ordinary scheduling.
* 19 November 2026: `FRUST_HOME` is honoured again. It replaces the target and artifact roots for one invocation, ahead of `config.json`, as in the 0.4.x line. The embedded copy of `paths.rs` in the central-target patch is updated with it.
* 19 November 2026: Cargo patch `0006-frust-lazy-libgit2.patch`. libgit2, and the system CA bundle load that comes with its TLS setup, now initialize on first git use instead of at the start of every Cargo invocation. Git dependencies, `cargo new`, `cargo fix` and proxy detection through git config still initialize it first, and the owner-validation setting is applied before the first repository is opened.
* 19 November 2026: Cargo patch `0008-frust-direct-entry.patch`. In the packaged toolchain `fcargo` is a hard link of the patched `cargo`, so plain non-terminal invocations skip one process launch. The terminal progress launcher is installed as `fcargo-ui`, and interactive terminals, static progress, an explicit `FRUST_CARGO` and a `+toolchain` argument still go through it. The direct entry is compiled for Linux and macOS.
* 19 November 2026: Cargo patch `0007-frust-unit-cache.patch`, a verified content-addressed unit artifact cache. Finished rustc outputs of registry, git and directory-source library and proc-macro units are stored under `units/v1` in the artifact cache root and restored in place of rustc on a verified hit. On by default under `fcargo`. Heavy build script runs of registry and git packages (3000 ms by default) are cached in the same store, keyed on their environment, tools and system directories and bound to the same target path. Controls are `FRUST_UNIT_CACHE`, `FRUST_UNIT_CACHE_MAX_BYTES`, `FRUST_UNIT_CACHE_SCRIPT_MIN_MS` and `FRUST_UNIT_CACHE_DEBUG`. Any mismatch or store problem compiles normally. Covered by 86 unit tests in the two cache modules and the 23-case `qualification/prove-unit-cache.py` probe, and exercised with the existing cooperative-build and managed-retention proof scripts. Linux x86-64 is the only verified host. The macOS code paths are compiled in and unverified. Measured results are not recorded yet. See `docs/how-it-works.md` for the key, limits and fallbacks.
* Compiler patch for post-LTO object reuse and RELR packing, kept as the candidate `frust-cg1` and not qualified. One ripgrep leaf sample ran at 0.655 of stock wall time, and single samples of the syn leaf, regex leaf and fd holdout were slower than stock. Both pieces are opt-in through `FRUST_EXPERIMENTAL_CODEGEN_CACHE=1` and `FRUST_EXPERIMENTAL_RELR=1`. Default compilation and linking retain upstream behavior.
* Continuous integration for the Rust workspace, the shell scripts, the runtime corpus and the pinned upstream patches, and manual `Toolchains` and `Benchmark` workflows that build the control and fRust stage-2 toolchains and run the paired qualification samples and release gates on GitHub's Linux x86-64 runners. The workload commits and the three generated `Cargo.lock` files are recorded in `qualification/cohorts.lock` and `qualification/locks/`.
* Qualification protocol, frozen workload cohort, baselines, edit scripts, runtime corpus, a parity proof script and compatibility record under `qualification/`.

### Changed

* The workspace packages are `frust-rs` (the installer binaries, replacing the 0.4.0 accelerator on crates.io) and `frust-rs-core`, version 0.5.0, licensed MIT to match `LICENSE`. `cargo install frust-rs` installs `frust-install`, `fcargo` and `frust-gates`. Pushing a `v*` tag runs the release workflow, which publishes the GitHub release and then the crates.

* Cargo skips checksumming excluded dependencies and treats dependencies saved during compiler invocation as dirty on the next build. Upgrading invalidates the earlier dependency checksum records.
* Qualification requires the complete frozen cohort and rejects missing tail series and invalid timings. The parity fixture now compiles and links a native archive and tests an edit between compiler completion and Cargo's fingerprint write.

### Dropped

* Pre-optimization fat-LTO cache. The full paired gate failed the holdout, runtime and stripped-size budgets, with the runtime and size gates also short of three workloads.
* Two-thread fat-LTO machine codegen. One fd holdout pair ran at 21.19s against 15.90s with 1.62 times the peak RSS, so the paired gate was not run.
* Fat-LTO pre-link at opt-level 1, and at opt-level 0 with an x86 machine outliner. Single samples did not reach the 0.80 leaf budget, and the opt-level 0 variant used about 2.11 times the control's peak RSS.
* Safe identical-code folding (`--icf=safe`). The ripgrep output was byte-identical to stock.
* Hot-cold splitting and scheduling retargets (`-Z tune-cpu`) were checked with the stock compiler and not adopted.

### Hardening

* Hardening fixes to the tools and documentation are in progress on this branch. The commit history has the specifics.

## Earlier accelerator releases

The sections for 0.3.0, 0.2.0, 0.1.1 and 0.1.0 describe the earlier fRust build accelerator, a compile-cache wrapper that is no longer in the active branch. They do not describe the compiler toolchain.

## [0.3.0] (earlier accelerator)

### Changed

* Concurrent cold builds share matching compiler units while retaining independent profile locks and original compiler flags.
* Concurrent benchmarks select dev, release, thin or fat profiles with native compiler-flag and executable-byte checks.
* Explicitly incremental optimized profiles share completed artifacts, including ThinLTO and fat LTO, while rustc retains its native sessions.
* Cache format 5 stores verified metadata ranges inside their backing archives instead of duplicating metadata payloads.
  Formats 3 and 4 remain readable. Public cache metadata types gain fields and an encoding variant.
* Deferred stores use a bounded broker queue and shared execution slots on Linux and macOS, with the existing helper
  and inline fallbacks. Multiple cache misses start broker setup during compilation; single-unit builds use the helper.
* Compression scratch buffers follow artifact size while retaining the existing encoded frames and verification rules.
* Output commitment hashing reuses bounded thread-local scratch storage instead of allocating a fresh block per file.
* Fixed-persona launchers specialize shared dispatch code, reducing executable size while preserving renamed entrypoints.

### Fixed

* Native incremental session maintenance preserves the compiler's current completed session and held working sessions.
* Retired cache entries prune their manifest and unit references; interrupted atomic records remain eligible for cleanup.
* Garbage collection preserves execution-slot lock identities so concurrent stores retain their global worker bound.
* Metadata proof generation reuses verified encoder input and avoids redundant archive scans and temporary writes.
* Native linker diagnostics retain actual invocation evidence and reject unsupported or ambiguous measurements.
* Windows MSVC cache hits restore unsuffixed PDB companions and bypass older incomplete entries.
* macOS store transport validates inherited socket roles and closes every rejected transferred descriptor.
* Target maintenance preserves files owned by pending stores and retries their cleanup after publication.

## [0.2.0] (earlier accelerator)

### Added

* `FRUST_CACHE_COMPRESSION=auto` stores compressible cache payloads as bounded LZ4 blocks on Linux filesystems
  without copy-on-write clones. `off` writes raw payloads; both settings can restore existing raw and compressed
  entries. Cache budgets account for the payloads' stored allocation.
* Opt-in `FRUST_LINK=smart` uses Apple's native linker directly for eligible macOS incremental development binaries,
  retaining static Rust linkage and the current incremental state. Explicit linker settings, custom drivers, tests,
  release builds and unsupported invocations keep their original link path.
* Release benchmarks cover ordinary release, ThinLTO and full/fat LTO, with real function-body edits, alternating
  build order, raw samples, actual compiler invocation audits, same-path binary identity and runtime output checks.
  Growth benchmarks compare combined target and cache allocation with compression enabled and disabled. Native-link
  profiling measures real application edits and separates the current binary's native-link phase from total build time.

### Changed

* Encoded cache manifests use format 4 so older readers do not restore compressed payloads as compiler outputs.
  Existing format-3 raw manifests remain readable.
* Completed metadata-only incremental reuse data with unknown ownership can expire after the configured idle
  retention (7 days by default), without cache coverage or a permanent newest-directory exemption. Target pressure
  can reclaim safely aged metadata reuse state after an hour with the defaults; fresh and uncertain state stays.
* Daily automatic GC retires eligible central-target payloads after 30 idle days. Foreground sampling is bounded;
  the detached collector drains eligible targets with a complete activity scan, and `FRUST_GC_SYNC=1` drains inline.
  Target/profile directories, `deps` and `.fingerprint` roots, and Cargo lock files remain; unknown or user-managed layouts
  are skipped. Cache and target budgets
  remain separate; the target budget is soft and does not impose a universal disk cap.
* Release publication requires the tag to match the package version and the latest main-branch push CI run for the
  exact release commit to have completed successfully before any release binaries are built.

### Performance

* Bounded, jobserver-aware workers hash source and dependency inputs with ordered key assembly and balance uneven
  input sizes. Small input sets stay sequential, and large-file hashing no longer starts a separate Rayon pool.
* Compaction creates cache-coverage indexes only when needed and reuses memoized file information. Deferred stores
  reserve helper capacity before preparing outputs, avoiding duplicate preparation when the helper pool is full.
* Queued builds wake on Linux inotify and macOS kqueue notifications, retaining FIFO checks and timed polling as
  fallbacks. Follower logs flush in bounded batches instead of flushing every record.
* Display-enabled builds defer unit-graph planning until the first display tick and poll it alongside compiler output.
  Short builds avoid the extra Cargo process, and planning is cancelled before the build lock is released. Target
  selection, tuning and lock scoping share an invocation-local snapshot of Cargo configuration.

### Fixed

* Restored output timestamps use the Unix filesystem clock, preventing fast cache hits from making Cargo's subsequent
  dependency and build-script timestamps older than their restored inputs.
* Successful metadata-only compiler probes can cache exact un-hashed output names; failed capability probes continue
  to run the compiler. Current inputs and stored payloads are checked before a cached result is accepted.
* Cache keys include explicitly selected linker executables and native inputs, with unreadable inputs bypassing the
  cache and changes during compilation preventing storage.
* Build locks are explicitly released even while forked processes retain cloned descriptors, so process startup cannot
  prolong a completed build's ownership.
* Due compaction runs after Cargo exits and before the build-lock handoff, so queued builds cannot continually take
  the profile before cleanup. Incomplete passes retry after 30 seconds instead of the completed-pass 10-minute
  interval; retries and cheap skips preserve the last successful pass time for hourly age reassessment.
* Crashed incremental working sessions and orphan session locks can be reclaimed after a one-hour grace using
  compiler-compatible guards and identity revalidation. Recent, held, malformed and uncertain state is retained.
* Idle-target cleanup first prunes expired finalized incremental reuse data in bounded batches, allowing progress
  through long build histories. Held compiler sessions protect their state and all program outputs; remaining
  program-output retirement requires joint admission of every profile. Session payloads are removed before locks,
  and preserved structural roots and activity times allow safe retries after partial removal.
* Maintenance reserves every canonical Cargo profile lock name, including absent alternatives, and profile
  retirement preserves their identities. Structural symlinks and unsafe central paths decline cleanup;
  nested native-output symlinks do not prevent unrelated cleanup and are not followed outside the managed tree.
* Benchmark no-op checks require zero actual compiler calls, Cargo compile notices and cache events. Warm restores
  distinguish declared Cargo-unit compilations from build-script capability probes, and unavailable audits remain
  unknown rather than being reported as zero. Runtime output checks cover every measured executable launch.

## [0.1.1] (earlier accelerator)

### Fixed

* The terminal progress signal (OSC 9;4) now carries an exact percent. When the display is on, frust asks cargo for its
  unit graph under the build lock (about 0.1 s, bounded to 5 s, and skipped for subcommands whose stream does not match
  it), so the bar fills from 0 to 100 instead of looping or stopping at an old estimate.
* The live display is redrawn as a fine line bar with half-cell steps, percent, exact unit count, elapsed time and cache
  hits, and unit timers aligned at the right edge. The animated Pac-Man bar is gone.

## [0.1.0] (earlier accelerator)

### Added

* Shared compile cache, central target directories, cooperative build lock and build display.
* Native Windows support alongside macOS and Linux, through a platform layer in `src/sys/` with Unix and Windows
  backends: build locks with `LockFileEx` on a sentinel byte, Ctrl-C reported as exit code 130, a job object around
  cargo, ANSI virtual terminal processing, no POSIX mode bits (access follows inherited ACLs) and copy-on-write
  clones on ReFS and Dev Drive. Windows support is new and verified by CI.
* Prebuilt binaries for Linux, macOS and Windows, `install.sh`, `install.ps1` and `cargo binstall` support.
* Continuous integration on all three platforms, an MSRV build and a job that runs the tests with
  `FRUST_CACHE_TRUST_STAMPS=audit`.
* Stamp-validated direct mode: manifests record a file stamp per input, and an input whose stamp is unchanged is
  accepted without being read. `FRUST_CACHE_TRUST_STAMPS` selects `on` (default, trusted inputs are randomly
  audited), `audit` (every trusted input is compared with a fresh content hash) or `off`. A stamp that lies is logged
  under `stamp-trust/`, makes the compile run and quarantines the volume for 7 days. Network, FAT, FUSE and unknown
  filesystems are never trusted; on Windows only NTFS and ReFS are. `frust doctor` gains a `stamp trust` check.
* Per-pre-key single-flight: processes missing on the same unit take a lock file under `keylocks/`, and a waiter
  restores the finished entry instead of recompiling. It fails open, and `FRUST_KEYLOCK_WAIT_SECS` (default 300)
  bounds the wait.
* Deferred store: a detached helper process finishes storing a compile result after rustc exits, so cargo does not
  wait for it. `FRUST_STORE_SYNC=1` stores inline.
* `frust self-update` with `--yes`, `--check` (exit code 10 when an update exists), `--version`, `--disable` and
  `--enable`. It downloads the release archive, verifies its SHA-256, confirms the staged binary's `--version` and
  replaces all eight binaries atomically with rollback. Copies installed with cargo or Homebrew get the matching
  update command instead. The command is not `frust update` so that `cargo update` keeps working.
* A one-line update notice after a successful build, from a release check that runs at most once a day in a detached
  process, only on an interactive terminal and never in CI or with `FRUST_BYPASS`. `FRUST_UPDATE_CHECK=off`
  disables it. `FRUST_UPDATE_API_URL` and `FRUST_UPDATE_DOWNLOAD_BASE` point the updater at a mirror, for mirrors and
  tests.
* `FRUST_GC_SYNC=1` runs the daily automatic gc inline.
* `frust bench --runs N` (default 5, 3 with `--quick`, minimum 3), median and min-max spread per row, a warm-up
  build, alternating tool order, separate gc and compaction rows, a display-on row and machine context in the report.

### Changed

* Automatic compaction stays synchronous but runs at background CPU and I/O priority, is skipped when a change
  signature shows nothing changed since the last complete pass, remembers file hashes and incompressible files in
  `compact-memo/`, judges compressibility from a sample and stops after a work budget (2 seconds or 512 MiB per
  pass), leaving the rest to the next pass.
* The daily automatic gc runs in a detached background process and sizes cache entries from their recorded byte
  counts, measuring them only when the cache is over its budget. An explicit `frust gc` still measures every entry.
* The manifest format is now 3. Manifests in other formats are ignored and rewritten, and their cache entries stay
  reachable for gc.
* `frust bench` exits with code 1 when a byte-identity or output check fails.
* Evicting a cache entry is a single rename; its files are deleted later.

### Fixed

* Compaction on Linux recognises dead incremental directories of binaries by reading the codegen-unit names in the
  DWARF data of ELF files, so they are removed even though their objects are gone.
* On Windows, binaries share one fingerprint directory across feature sets, so only the older incremental directories of a
  crate are treated as superseded.
* The toolchain probe runs one query before the parallel ones. Before, the first use of an uninstalled toolchain named
  by `rust-toolchain.toml` made rustup auto-install it from several processes at once, which left it without `cargo`.
* Dependency files for profile directories whose path contains spaces are rewritten and restored correctly.
* A non-UTF-8 environment variable no longer aborts the wrapper.

### Performance

* Large files in the immutable registry and git source trees are hashed through a memory map on Unix (with rayon for
  files of 4 MiB or more). Everything else is read, because an editor truncating a mapped file in place would fault.
* Candidates are verified newest-modified first, in parallel for large ones, and a candidate whose cache entry is
  gone is skipped before any input is hashed.
* Manifests are not rewritten when nothing changed.
* The lock hand-off between builds is faster: quicker waiter polling, a build log written in batches, coalesced
  display redraws and an event feed read only when it has grown.
