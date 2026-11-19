# Distribution-optimized fRust on Linux

The earlier [CPU and disk measurements](CPU-DISK-BENCHMARK.md) used a release-optimized Cargo and a source-built stage-2 compiler. Official Rust 1.100.0 uses additional profile training and post-link optimization. The two compiler builds therefore start from different host optimization levels.

[The optimized Linux build driver](../support/optimize-linux.sh) runs the pinned Rust checkout's own `opt-dist local` pipeline. It uses ThinLTO, one codegen unit for rustc and the standard library, line information for the standard library, clang and LLD, jemalloc for Rust toolchain allocations, vendored native Cargo dependencies, and PGO for rustc, Cargo, rustdoc and Clippy. The LLVM link uses the pinned CI artifact. On this machine its SHA-256 also matched the installed official Rust LLVM library, `517f2e2d3762ccd6fe742c1b2fc720f447777c2589c99f58e7d12bfacf7b908c`.

## Build and compare

Use an isolated checkout of the Rust pin with the intended Cargo and rustc patches applied. Initialize the pinned Cargo, LLVM, backtrace and rustc-perf submodules and the documentation submodules used by the training run. Provide clang, LLD and a `strip` with `--keep-file-symbols` on `PATH`. BOLT requires those file symbols for split functions. The host LLVM tool directory must provide working `bin/llvm-bolt`, `bin/merge-fdata` and `bin/llvm-profdata`. Use the pinned LLVM's profiler reader for compatible instrumentation data. The script builds in its specified work directory and uses that directory for temporary profiles.

```sh
bash support/optimize-linux.sh "$rust_source" "$work_directory" "$host_llvm_directory"
```

The pipeline builds instrumented tools, trains them on the pinned rustc-perf suite, merges the profiles, rebuilds with PGO, then instruments and optimizes the rustc driver with BOLT. It requests the full host library during opt-dist's bootstrap builds so training can compile proc macros and test binaries. It reuses the already optimized shared LLVM and produces distribution archives. Check the final archive binaries and training logs before using the result in a benchmark.

Use the [build and disk collector](cpu-benchmark.py) with official Rust and the optimized package on the same core set. Incremental allocation includes Cargo's flat and per-unit layouts and counts each inode once. The [report's strict screen](report-cpu-disk.py) checks each measured build case against official Rust. It requires a median paired CPU ratio below one, target allocation at or below official Rust, and a paired runtime CPU upper bound at or below 1.01. A pass describes the measured cases and host.

The local `frust-0ef1bc386-dist-bolt1` package completed all nine upstream workloads for compiler PGO and again for BOLT. Its driver contains `.note.bolt_info`, and the packaged LLVM digest matches official Rust 1.100.0. LLVM's `malloc` binds to the binary's compiled-in jemalloc. Codegen parity passed nine cases and the release-default check passed 22 cases. The package and verification logs are under `target/compiler-rewrite/toolchains/frust-0ef1bc386-dist-bolt1` and `target/compiler-rewrite/optimized-dist/dist-*`.

## Matched measurements

The Linux x86-64 matrix contains 1,476 measurements across 16 ripgrep, fd and oxipng configurations. It covers debug and project release, LTO off, ThinLTO, fat LTO, incremental and nonincremental builds, cold and fresh builds, history, no-ops, clean and clean rebuild. Each expensive build cell has one observation. No-ops use seven pairs and each runtime cell uses 30 pairs pinned to a single core. CPU seconds sum user and system time for Cargo and its waited descendants. Allocated bytes measure the target tree, separate from the installed package.

| Build operation | Configurations faster than official | Median fRust / official CPU |
| --- | ---: | ---: |
| Cold | 5 of 16 | 1.027 |
| Semantic edit | 1 of 16 | 1.033 |
| No-op | 1 of 16 | 1.041 |
| Clean rebuild | 1 of 16 | 1.038 |

The fastest cold result was fd with incremental builds and LTO off, at 0.866 times official Rust's CPU. Oxipng's project-release edit used 1.216 times official CPU. That configuration also retained an incremental store, at 83.10 MiB after the edit versus 56.14 MiB for official Rust. The explicit matched profiles generally used very similar target space. All 224 timed no-ops reused every unit and all 32 cleans removed the measured target allocation. Cold-build stripped binaries ranged from 0.996 to 1.0014 times official size.

All 480 executable runtime pairs produced matching application output. Eleven of 16 configurations cleared the one-sided 1% runtime limit in the original matrix. The longer oxipng LTO-off rerun cleared it at 1.0051. The project-release rerun had a 1.0209 bound, and the same-binary control had a 1.0183 bound. That cell remains unqualified at 1%. The strict screen recorded 248 failures in 324 checks across build CPU, exact allocated-byte comparisons and runtime. The matrix and separate reruns are under `target/compiler-rewrite/optimized-dist/bolt-cpu-*` and `bolt-runtime-repeat-*`.

The local launcher was also built with fat LTO, one codegen unit and PGO from the storage probe. It occupied 845,496 bytes compared with 1,007,000 for the release launcher. Thirty paired no-ops in each mode gave CPU ratios of 0.997 in both plain and static modes. This experiment establishes a smaller launcher and near-parity CPU for the measured no-ops. Its samples are in `target/compiler-rewrite/optimized-dist/launcher-noops.jsonl`.

The PGO-only diagnostic isolated no-op overhead on a one-crate target. Across 30 alternating rounds on one core, median CPU was 36.65 ms for official Cargo, 38.94 ms for PGO fRust Cargo invoked directly, and 41.18 ms through the PGO launcher. All 90 samples reused the existing compilation. The paired launcher/direct difference was 2.30 ms. A separate syscall trace recorded two executable loads through the launcher and one in either direct invocation. Direct fRust Cargo still used 5.8% more CPU by the paired median. Samples and traces are under `target/compiler-rewrite/optimized-dist/pgo-noop-*`.

[The in-process Cargo entry trial](fast-cargo-entry.patch) hardlinks the patched Cargo as `fcargo` and places the existing launcher at `fcargo-ui`. Plain packaged invocations can enter Cargo directly. Static progress, explicit Cargo selection and rustup toolchain directives still go through the launcher. The prototype passed build-script Cargo identity, explicit compiler selection, rustup delegation, static rendering and recursion checks.

On the source-built Cargo used in this trial, 100 paired no-ops took 42.09 ms median CPU through the launcher and 39.83 ms through the in-process entry. The paired ratio was 0.950 with a one-sided 95% bound of 0.955. Official Cargo used 35.72 ms median CPU in the same run. The forced-UI path adds another process launch and still needs its own performance and terminal qualification. The trial stays outside the default patch series. Raw data is in `target/compiler-rewrite/optimized-dist/cargo-fast-entry-noops-final.jsonl`.

Two repeated ripgrep cold builds with LTO off, each compiling 44 Cargo artifacts, investigated a larger preliminary regression. Official Rust used 74.96 and 73.29 CPU seconds. The BOLT package used 78.12 and 77.48 CPU seconds. Per-compiler timings showed the two biggest increases in `regex_automata` and `rg`, averaging 0.75 and 0.65 CPU seconds respectively. A separate single-core trace put most of the difference in LLVM optimization and emission. Both compilers used the same LLVM binary and the same requested profile. Raw measurements are under `target/compiler-rewrite/optimized-dist/ripgrep-off-attribution.jsonl` and `ripgrep-compile-*.phases`.

An allocator tuning replay tested those two expensive compiler invocations directly. Three pairs per crate produced byte-identical outputs. Median tuned CPU was 0.996 times default for `regex_automata` and 1.004 for `rg`. The latter's peak process RSS rose 12.0%. The existing allocator configuration remains the measured baseline.

## Automatic storage

[The storage probe](automatic-storage.py) creates six feature variants per mode in isolated workspaces and then verifies that the last variant remains fresh. It exercises normal noninteractive output, explicit plain output, quiet output, JSON, static progress, disabled collection, and an explicit target override. The allocated byte count comes from the actual target filesystem.

The pinned Cargo stores units under `debug/build/<package>/<unit>/`. The existing wrapper looks for a flat profile with `deps/` and `.fingerprint/`. The baseline retained all six unit executables, and its liveness ledger stayed absent in every mode.

[The managed-target retention patch](../support/patches/0005-frust-managed-unit-retention.patch) runs inside Cargo by default on Linux. It tracks executable builds whose binaries Cargo has published outside their unit directory. Each binary keeps the active version and three inactive versions. Incremental data for these units lives alongside the executable copy it belongs to. A repeated build of a known configuration skips the directory scan. Collection requires a managed build directory, the pinned per-unit layout, implicit native build kinds, and an exclusive Cargo lock. Shared-lock cooperative checks defer collection. Test binaries, dependencies, build-script outputs and explicit target or build directories use regular Cargo behavior. Set `FRUST_GC=0` to keep inactive variants.

| Mode | Baseline units / allocated | Retention units / allocated |
| --- | ---: | ---: |
| Noninteractive, plain, quiet, JSON and static | 6 / 3.69 MiB | 4 / 2.49 MiB |
| Cleanup disabled | 6 / 3.69 MiB | 6 / 3.71 MiB |
| Explicit target directory | 6 / 3.69 MiB | 6 / 3.69 MiB |

The active executable produced the expected output on each revision and remained fresh on the final invocation. The managed target used 32.6% fewer allocated bytes in the six-variant probe. This is a per-package retention result. Cross-workspace budget eviction and the concurrent-check patch combination need separate qualification. Raw samples are in `target/compiler-rewrite/optimized-dist/storage-verified-*.jsonl`.

A second probe compiled an application with a build script and a feature-sensitive path dependency. The current and previous retained configurations reused all three Cargo artifacts without compiling again. The executable verified both the build-script value and the selected dependency feature. Its log is `target/compiler-rewrite/optimized-dist/storage-dependency-proof.log`.

The 20-variant stress run compared the retention trial with official Rust 1.100.0 on Linux x86-64 and ext4. Each toolchain completed 140 builds and seven verified no-ops. The table shows static progress for fRust and the corresponding direct Cargo run. Normal noninteractive, plain, quiet and JSON calls produced the same unit counts.

| Toolchain | Retained executables | Allocated target | Of which incremental |
| --- | ---: | ---: | ---: |
| Official Cargo | 20 | 89.41 MiB | 2.82 MiB |
| fRust trial with cleanup disabled | 20 | 12.46 MiB | 2.97 MiB |
| fRust trial with automatic cleanup | 4 | 2.52 MiB | 0.60 MiB |

Automatic cleanup reduced target allocation by 79.8% against the same trial with cleanup disabled. The managed trial occupied 97.2% less target space than official Cargo for this fixture. The two compilers produce debug executables of different sizes, which accounts for much of the cross-toolchain difference. The fRust trial used a source-built compiler and the optimized launcher. The data is in `target/compiler-rewrite/optimized-dist/storage-20-*.jsonl`.

The final retention patch repeated all 147 fRust samples with the same unit counts and 2.52 MiB in all five managed output modes. [The lock and retention proof](prove-managed-retention.py) verified that a shared lock delayed an exclusive build and its cleanup. The build then reclaimed old units, kept test artifacts and a different binary in the same package, preserved unrelated data, and kept the preceding configuration fresh. The one-job cooperative suite passed identical, overlapping, interrupted, edited and conflicting requests. Logs and samples are under `target/compiler-rewrite/optimized-dist/storage-published-*`.

With the BOLT compiler and its upstream-matched standard-library settings, the same verified retention Cargo used 17.96 MiB with automatic cleanup and 89.64 MiB with cleanup disabled. Official Cargo used 89.41 MiB on the same fixture. All five managed output modes retained four of 20 configurations and all seven final no-ops stayed fresh. The lock proof passed again with the optimized compiler. Samples are under `target/compiler-rewrite/optimized-dist/bolt-with-retention-*`.

The 16-configuration benchmark package uses the first three committed Cargo patches, the committed rustc patch, and the [initial retention trial](managed-unit-retention.patch). It uses explicit target directories for the matched CPU comparisons. The pipeline uses the upstream Linux distribution's static Cargo dependency setting. The older Cargo training workload still requires OpenSSL development headers from the host or an isolated SDK.

## Final managed package

`frust-0ef1bc386-dist-bolt-managed1` combines the verified BOLT compiler and sysroot with Cargo built from the committed cooperative-check and final retention patches. Cargo was rebuilt with ThinLTO, one codegen unit, static native dependencies and profile-use from the merged training data. Its Cargo snapshot is `2abdf6792160108f7e51d1f042204b228594443f`. The package contains local source trees and passed a relocation test under a path with spaces, including a native build and a vendored offline `build-std` check.

The final package passed nine codegen parity cases, 22 profile-default checks, all 147 storage observations and the five cooperative-check cases. The 20-configuration storage result remained 17.96 MiB with default cleanup and 89.64 MiB with cleanup disabled. A separate three-way benchmark recorded 267 measurements for ripgrep dev and incremental fat LTO. These are CPU seconds for the two end points of that comparison.

| Profile | Official cold | Final cold | Official edit | Final edit | Final runtime upper 95% |
| --- | ---: | ---: | ---: | ---: | ---: |
| Debug | 28.88 | 25.85 | 0.601 | 0.597 | 1.0062 |
| Incremental fat LTO | 50.87 | 56.38 | 26.02 | 26.21 | 1.0014 |

The final package cleared the 1% runtime limit in both profiles. Its no-op CPU medians were 28.7 ms in debug and 29.2 ms with fat LTO, versus 25.4 ms and 26.3 ms for official Rust. The strict combined screen recorded 24 failures in 34 checks. The local package is under `target/compiler-rewrite/toolchains/frust-0ef1bc386-dist-bolt-managed1`. Its logs and measurements are under `target/compiler-rewrite/optimized-dist/managed-pgo-*`.
