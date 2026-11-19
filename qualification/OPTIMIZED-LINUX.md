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

## Unit artifact cache, build script cache and Cargo startup

Measured on 19 November 2026 on the same Intel Core Ultra 7 255H host, after the cache patches ([0006 lazy libgit2](../support/patches/0006-frust-lazy-libgit2.patch), [0007 unit and build script cache](../support/patches/0007-frust-unit-cache.patch), [0008 direct entry](../support/patches/0008-frust-direct-entry.patch)). The package is `frust-0ef1bc386-dist-bolt-uc4` (the table below was measured on `uc3`, which differs only in how restored files are timestamped; `fd` dev and `ripgrep` release were repeated on `uc4` with cold medians 1.006 and 1.020, clean rebuild 0.024 and 0.437, no-op 0.908 and 0.913): the verified BOLT compiler and sysroot of `managed1` with a Cargo built from the full patch series with ThinLTO, one codegen unit, static native dependencies and the merged compiler profile. Its Cargo snapshot is the working tree used for patches 0001 to 0008.

**Method change.** The earlier matrix let Cargo's jobs migrate between P and E cores, which moved CPU seconds by several percent between identical runs. [The paired store benchmark](run-store-bench.py) pins every build of both toolchains to four E-cores (`taskset -c 6-9`, four jobs), alternates the order of the toolchains between rounds, and uses a fresh store for every cold build. It compares official Rust 1.100.0 with fRust at the same locked sources. With the cache disabled for fRust the pinned cold ratio on fd was 1.000 in the first pilot, so the earlier +2.7% median was mostly core placement. Another session was running its own benchmarks on the machine during these runs, so single rounds can still be disturbed; medians and minima of three rounds are shown. Raw samples are under `target/compiler-rewrite/probes/store-bench/` (`rg-rel3`, `rg-dev3`, `ox-dev3`, `fd-dev3`, `rg-dev-off`).

Ratios are fRust CPU divided by official CPU (below 1 is faster). Cold uses an empty target and an empty store. Clean rebuild removes the target and rebuilds with the store populated by the cold build. No-op is the median of five.

| Application, profile | Cold median / min | Clean rebuild median / min | No-op median / min |
| --- | ---: | ---: | ---: |
| ripgrep, release | 1.021 / 1.078 | 0.437 / 0.463 | 0.889 / 0.905 |
| ripgrep, dev | 1.027 / 1.026 | 0.331 / 0.330 | 0.900 / 0.886 |
| oxipng, dev | 1.022 / 1.023 | 0.304 / 0.304 | 0.893 / 0.896 |
| fd, dev | 1.020 / 1.002 | 0.024 / 0.024 | 0.903 / 0.905 |

- **Clean rebuild.** fd's clean rebuild used 2.7 CPU seconds against 113.5 for official Cargo. All 59 registry units were restored and the 33-second `tikv-jemalloc-sys` build script (about 99 CPU seconds of C compilation) was restored from the build script cache. The other cohorts still compile their workspace crates. The stored output is byte-identical to a fresh build for every cached unit (probe case `byte-identical`).
- **No-op.** The no-op is about 10% cheaper than official Cargo: lazy libgit2 removes the CA bundle parse (about 45 reads of 4 KiB per invocation) and the direct entry removes a process launch.
- **Cold, empty store.** fRust is still 1.5 to 2.5% above official Rust. This is not the cache: with the cache switched off the ripgrep dev cold ratio was 1.019 (median) and 1.012 (minimum). Cache bookkeeping on a cold ripgrep release build was 60 ms of wall time in total (statistics line under `FRUST_UNIT_CACHE_DEBUG=1`: key 9 ms, hashing 46 ms, manifest 2 ms for 145 MiB hashed, outputs hard-linked into the store without copying).
- **Compiler attribution.** Compiling `regex_syntax` (no dependencies, dev profile) on one pinned core, 8 alternating rounds: official 1.243 s, `bolt1` 1.278 s, `uc3` 1.264 s, PGO-only `pgo1` 1.299 s. For `regex_automata` the packaged compiler was 2.4% slower than official on the quiet rounds. `-Z time-passes` shows the extra time spread over front-end passes (type checking +8%, MIR borrow checking +6%, metadata generation +11%, monomorphization collection +12%) while LLVM passes differ by 1.6%. The LLVM library is the same file. The standard library rlib and rmeta files have identical sizes to the official ones. Process start-up differs by about 1.2 ms per compiler invocation. The remaining gap is therefore in the quality of the compiler front-end binary (profile or layout), not in the patches, the cache, or LLVM. Closing it needs hardware-counter attribution, which this host blocks (`kernel.perf_event_paranoid` is 4).
- **Proofs on the package.** On `uc4` the unit cache probe passed all 23 cases in three consecutive runs (including toolchain change with a second compiler, byte-identical restored outputs, vendored source edits, build script restores and corruption repair), the 97 cache unit tests passed, and the managed-retention and cooperative-build proofs passed. The probe found one defect only visible on the optimized Cargo: a restore finishing within the kernel's coarse timestamp granularity gave restored files a newer mtime than the stamp of their dependents, so every later build re-hit the cache without settling. Restored files now carry the unit's own invocation timestamp, which Cargo's comparisons treat as fresh. Two independent reviews of the cache modules found eleven defects (store write-through, vendored sources, split debuginfo, writes outside OUT_DIR, symlink steering, cross-compiler identity and others) and all were fixed with tests.
- **Unverified.** macOS, aarch64 Linux, cross targets, and the peer scenario (a cold build whose store was warmed only by a different application) which showed no benefit because the two applications share few identical units.

## Linux installer and ecosystem checks

Run on 19 November 2026 with the `uc4` package.

- **Installer smoke (Linux x86-64, non-root prefix containing a space).** Local archives of two package versions with sha256 files: install, upgrade, a native `fcargo run` from the installed prefix with `env -i` and a PATH of `/usr/bin:/bin` (no ambient Rust), rollback to the previous version, rollback again, uninstall, and rejection of a mismatched checksum before extraction (exit 1, nothing activated). All behaved as specified. The installed `fcargo` and `cargo` shared one inode, so the direct entry survives archive extraction.
- **Ecosystem tests.** [The comparison script](run-ecosystem-tests.py) runs `cargo test --workspace --no-fail-fast` on locked cohorts with official Rust, then with fRust on an empty unit cache, then with fRust after deleting the target (units restored). Aggregates of every `test result:` line were identical across all three runs: ripgrep 862 passed, 323 failed, 3 ignored; fd 287 passed, 1 failed; oxipng 296 passed, 0 failed. The ripgrep and fd failures also occur with official Rust on this machine, so they are inherited and not caused by fRust or the cache.
- **Not covered.** The upstream compiler, library and Cargo test suites and the MXFrame project were not run against this package, and nothing was run on macOS.
