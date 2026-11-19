# Distribution-optimized fRust on Linux

The earlier [CPU and disk measurements](CPU-DISK-BENCHMARK.md) used a release-optimized Cargo and a source-built stage-2 compiler. Official Rust 1.100.0 uses additional profile training and post-link optimization. The two compiler builds therefore start from different host optimization levels.

[The optimized Linux build driver](../support/optimize-linux.sh) runs the pinned Rust checkout's own `opt-dist local` pipeline. It uses ThinLTO and one codegen unit for rustc, clang and LLD, jemalloc for Rust toolchain allocations, vendored native Cargo dependencies, and PGO for rustc, Cargo, rustdoc and Clippy. The LLVM link uses the pinned CI artifact. On this machine its SHA-256 also matched the installed official Rust LLVM library, `517f2e2d3762ccd6fe742c1b2fc720f447777c2589c99f58e7d12bfacf7b908c`.

## Build and compare

Use an isolated checkout of the Rust pin with the intended Cargo and rustc patches applied. Initialize the pinned Cargo, LLVM, backtrace and rustc-perf submodules and the documentation submodules used by the training run. Provide clang and LLD on `PATH`. The host LLVM tool directory must provide working `bin/llvm-bolt`, `bin/merge-fdata` and `bin/llvm-profdata`. Use the pinned LLVM's profiler reader for compatible instrumentation data. The script builds in its specified work directory and uses that directory for temporary profiles.

```sh
bash support/optimize-linux.sh "$rust_source" "$work_directory" "$host_llvm_directory"
```

The pipeline builds instrumented tools, trains them on the pinned rustc-perf suite, merges the profiles, rebuilds with PGO, then instruments and optimizes the rustc driver with BOLT. It requests the full host library during opt-dist's bootstrap builds so training can compile proc macros and test binaries. It reuses the already optimized shared LLVM and produces distribution archives. Check the final archive binaries and training logs before using the result in a benchmark.

Use the [build and disk collector](cpu-benchmark.py) with official Rust and the optimized package on the same core set. Incremental allocation includes Cargo's flat and per-unit layouts and counts each inode once. The [report's strict screen](report-cpu-disk.py) checks each measured build case against official Rust. It requires a median paired CPU ratio below one, target allocation at or below official Rust, and a paired runtime CPU upper bound at or below 1.01. A pass describes the measured cases and host.

The local launcher was also built with fat LTO, one codegen unit and PGO from the storage probe. It occupied 845,496 bytes compared with 1,007,000 for the release launcher. Thirty paired no-ops in each mode gave CPU ratios of 0.997 in both plain and static modes. This experiment establishes a smaller launcher and near-parity CPU for the measured no-ops. Its samples are in `target/compiler-rewrite/optimized-dist/launcher-noops.jsonl`.

## Automatic storage

[The storage probe](automatic-storage.py) creates six feature variants per mode in isolated workspaces and then verifies that the last variant remains fresh. It exercises normal noninteractive output, explicit plain output, quiet output, JSON, static progress, disabled collection, and an explicit target override. The allocated byte count comes from the actual target filesystem.

The pinned Cargo stores units under `debug/build/<package>/<unit>/`. The existing wrapper looks for a flat profile with `deps/` and `.fingerprint/`. The baseline retained all six unit executables, and its liveness ledger stayed absent in every mode.

[The managed-target retention patch](managed-unit-retention.patch) runs inside Cargo. It takes live unit paths from the completed graph while Cargo holds the exclusive build-directory lock. It keeps all active units and three inactive versions per package visited by the build. Incremental data lives alongside the unit it belongs to. Repeated builds of a known root use a marker to skip the directory scan. The collection path requires a managed build directory, the pinned per-unit layout, implicit native build kinds, and the exclusive lock. Explicit targets and fine-grained locking use their regular paths.

| Mode | Baseline units / allocated | Retention units / allocated |
| --- | ---: | ---: |
| Noninteractive, plain, quiet, JSON and static | 6 / 3.69 MiB | 4 / 2.49 MiB |
| Cleanup disabled | 6 / 3.69 MiB | 6 / 3.71 MiB |
| Explicit target directory | 6 / 3.69 MiB | 6 / 3.69 MiB |

The active executable produced the expected output on each revision and remained fresh on the final invocation. The managed target used 32.6% fewer allocated bytes in the six-variant probe. This is a per-package retention result. Cross-workspace budget eviction and the concurrent-check patch combination need separate qualification. Raw samples are in `target/compiler-rewrite/optimized-dist/storage-verified-*.jsonl`.

A second probe compiled an application with a build script and a feature-sensitive path dependency. The current and previous retained configurations reused all three Cargo artifacts without compiling again. The executable verified both the build-script value and the selected dependency feature. Its log is `target/compiler-rewrite/optimized-dist/storage-dependency-proof.log`.

The local optimized retry uses the first three committed Cargo patches, the committed rustc patch, and the retention trial. Its compiler tools are isolated from the shared checkout. The first instrumented build reached Cargo and hit the host's missing OpenSSL development files. The retry uses the upstream Linux distribution's static Cargo dependency setting.
