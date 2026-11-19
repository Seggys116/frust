# CPU and disk by build profile

Measured on native Linux x86-64 and ext4 on 19 November 2026. This expands the [previous CPU matrix](CPU-BENCHMARK.md) with four successive edits, actual `cargo clean` runs, cold rebuilds after cleaning, an allocation breakdown, and more executable runtime pairs. The official compiler, source-built control and fRust package use pinned Rust 1.100.0. The fRust package is `frust-0ef1bc386-native-opt1`.

The [full tables](CPU-DISK-DETAIL.md) contain 3,549 measurements across 22 workload and profile combinations. All 462 no-op samples compiled zero units. All 48 clean runs reclaimed the entire measured target allocation. All nine fresh-versus-warm fat-LTO `.text` comparisons matched. The 2,520 runtime samples matched the expected application output within their workload and profile. Base fRust cleared the 1% runtime upper-bound screen against official Rust in 17 of 22 configurations. The detailed runtime table includes the five results above that bound.

The benchmark uses separate target directories, pinned source and lockfiles, offline builds, four Cargo jobs, and the same cores for the three toolchains inside each workload. The three workloads ran on different core sets on a shared host. CPU time sums user and system time for Cargo and its waited descendants. It is less sensitive to scheduler waits than elapsed time, and remains sensitive to core type, frequency and host contention. Compare build times within a workload and profile. Each large build cell has one observation. No-op medians use seven measurements. Runtime comparisons use 30 pairs on ripgrep and oxipng and 60 on fd.

The dev and project release rows use the project's own profile. The dev nonincremental row explicitly disables incrementality. Release mode comparisons use opt level 3, 16 codegen units, debug level 0 and no stripping at build time. The `off` profile disables LTO. The `thin` and `fat` profiles select ThinLTO and full fat LTO. Each is tested with incrementality enabled and disabled where listed.

All 66 workload, profile and toolchain combinations passed Cargo unit-graph validation. The explicit release modes used the requested LTO and incremental settings. Oxipng's dev profile uses opt level 2. Its project release requests fat LTO and strips symbols. fRust fills in 16 CGUs and incrementality there, while official Rust and the source-built control leave incrementality disabled. The `profile-graphs.json` files in the workload directories record the effective binary profiles.

## Reading the measurements

- **Cold** starts with an empty target. The OS file cache stays warm across invocations.
- **Edit** changes the produced program and verifies the new behavior. **Revert** restores the original behavior.
- **History** reapplies the edit, moves through two distinct behavior-changing revisions and reverts on the fourth build. Every build checks its executable.
- **Fresh edited** builds the same edited source in a second empty target. It also compares the nonempty ELF `.text` sections between warm and fresh builds. This check runs in the matched incremental fat-LTO profile.
- **Clean** times `cargo clean`, records target bytes immediately before and after it, and asserts the old executable is gone. **Clean rebuild** rebuilds the original source. These cases run on ripgrep and oxipng.
- **Runtime** measures the unedited executable after history and verifies each application output. Ripgrep searches a 32 MiB corpus sixteen times, fd walks 12,800 files, and oxipng compresses eight PNGs. Pairs alternate toolchain order. The one-sided 95% bound describes these observed pairs and tests. A bound above 1.01 leaves the 1% runtime screen open.
- **Allocated bytes** sum Linux filesystem blocks within the target, counting hard-linked inodes once. **Logical bytes** sum file lengths and directory entries from the same walk. Incremental, dependency, build-script and fingerprint storage are reported in the JSONL separately. Only the target tree is measured. This excludes installed toolchains, the shared registry and filesystem free-space accounting outside the target.

## Incremental build trade-offs

The matched 16-CGU incremental fat-LTO edit sample gives a direct comparison with native Rust. Values are CPU seconds and allocated target MiB.

| Workload | Official Rust | Source-built control | fRust |
| --- | ---: | ---: | ---: |
| ripgrep | 38.00 s / 219.45 MiB | 39.51 s / 174.37 MiB | 40.16 s / 174.37 MiB |
| fd | 45.63 s / 358.25 MiB | 54.92 s / 304.64 MiB | 45.65 s / 304.61 MiB |
| oxipng | 19.95 s / 102.16 MiB | 20.22 s / 81.59 MiB | 20.22 s / 81.58 MiB |

fRust's matched targets are smaller than official Rust's in these samples. The source-built control occupies almost the same space, so the measured savings track the source-built package baseline. Across the four-build history, the greatest fRust target growth above its cold seed was 0.168 MiB. The standing allocation cost of enabling incrementality was substantially larger in several profiles.

These pairs change the incremental setting within a build profile on the base fRust package. Each edit is a single observation. Allocation is measured after the edit.

| Workload and profile | Incremental off CPU | Incremental on CPU | Incremental off target | Incremental on target |
| --- | ---: | ---: | ---: | ---: |
| ripgrep dev | 5.41 s | 1.15 s | 187.87 MiB | 319.51 MiB |
| fd release, LTO off | 10.26 s | 0.70 s | 274.42 MiB | 291.97 MiB |
| oxipng release, LTO off | 3.58 s | 0.89 s | 55.80 MiB | 75.38 MiB |
| fd release, ThinLTO | 59.77 s | 65.90 s | 279.18 MiB | 311.56 MiB |

For release artifacts, direct comparisons interleaved fRust binaries from the three incremental LTO modes on one core per workload. The commands produced identical application output. All ratios use the fat-LTO binary as the reference. These are CPU ratios over 30 rounds for ripgrep and oxipng and 60 for fd.

| Workload | LTO off median / upper 95% | ThinLTO median / upper 95% |
| --- | ---: | ---: |
| ripgrep | 1.744 / 1.753 | 0.991 / 0.993 |
| fd | 1.102 / 1.112 | 1.059 / 1.069 |
| oxipng | 1.050 / 1.051 | 1.051 / 1.053 |

LTO-off incremental builds used much less CPU for these edits. Their executables used more CPU on all three measured commands. This trade-off matters for the default profile and for interpreting build-only results. Raw comparisons are under `target/compiler-rewrite/cpu-runtime-profiles-{ripgrep,fd,oxipng}`.

## Disk optimization trials

The post-LTO prototype now avoids writing bitcode for known partitions. It saved 87% and 94% of intermediate bitcode writes on semantic ripgrep and fd edits with near-parity CPU against the fused splitter. An exact-input shortcut also bypasses splitting and merging when the same optimized module returns. The measurements include only the backend replay stages. [LLVM replay measurements](LLVM-PARTITION-CPU.md)

Compressed DWARF was tested in real ripgrep dev builds on the same fRust package. Each codec used a separate paired run on one performance core. The modes compressed both emitted object sections and linked debug sections.

| Mode | Allocated target after history | Baseline target | Median history CPU | Baseline CPU | Runtime paired median / upper 95% |
| --- | ---: | ---: | ---: | ---: | ---: |
| zlib | 254.91 MiB | 319.50 MiB | 2.00 s | 1.13 s | 1.001 / 1.009 |
| zstd | 252.45 MiB | 319.50 MiB | 2.09 s | 1.12 s | 1.004 / 1.007 |

Both variants passed executable output checks and GDB resolved the same source line. The compressed builds used different Cargo rustflags and produced different executable `.text` hashes from their controls. The measured warm edit CPU increase keeps compression out of the default candidate. Raw measurements are under `target/compiler-rewrite/cpu-dwarf-{zlib,zstd}`.

## Compiler allocator trial

An environment-only diagnostic compared four distinct ripgrep warm fat-LTO edits, alternating the order between fRust and the same package with jemalloc loaded. All four runs compiled only `rg`. The edited `.text` digests matched between the variants. Median paired CPU was 0.879 times the base package. Cold CPU was 103.37 versus 108.22 seconds. Largest-process peak RSS was 497,300 versus 532,856 KiB on the cold run. This diagnostic used preloading for the full invocation and does not isolate which process benefited.

[The package preparation script](prepare-allocator-toolchain.sh) creates a separate ELF toolchain variant with a direct jemalloc dependency on `frustc`. The original `rustc` executable remains byte-identical. Loader diagnostics confirmed LLVM's `malloc` calls bind to the copied jemalloc library. The variant requires no preload environment and adds about 0.82 MiB of installed library data. The Ubuntu allocator build requires glibc 2.34. This is a local comparison package with that compatibility floor.

The package comparison uses the same fRust compiler code, standard library, Cargo and profiles on each side. The warm column is the median candidate/base CPU ratio across the edit, revert, and four history events. Cold columns are single observations.

| Workload | Profile | Base cold CPU | Allocator cold CPU | Warm CPU ratio | Runtime CPU median / upper 95% |
| --- | --- | ---: | ---: | ---: | ---: |
| ripgrep | Fat LTO, incremental | 113.82 s | 113.47 s | 0.887 | 1.000 / 1.001 |
| ripgrep | ThinLTO, incremental | 132.50 s | 128.40 s | 1.004 | — |
| ripgrep | LTO off, incremental | 89.30 s | 84.21 s | 0.909 | — |
| ripgrep | Dev | 49.62 s | 49.68 s | 0.981 | — |
| oxipng | Fat LTO, incremental | 187.58 s | 183.76 s | 0.921 | 0.990 / 1.005 |
| oxipng | Dev | 278.72 s | 274.53 s | 0.966 | 0.998 / 1.010 |

All 42 cold and edited executable pairs were byte-identical across the two packages. Each runtime row has 30 CPU pairs and matching application output. The exact oxipng dev upper estimate was 1.0097. The absolute target allocation difference had a 4 KiB median and a 20 KiB maximum. The gain is primarily compiler CPU. Results are under `target/compiler-rewrite/cpu-allocator-{ripgrep,oxipng}`.

The allocator package passed nine codegen parity cases and 22 release-default checks, including explicit profiles and cross-target selection. The proof logs are under `target/compiler-rewrite/logs/prove-allocator-*`.

A separate matched comparison used official Rust and the allocator package on oxipng with 16 CGUs, incremental fat LTO, two low-power cores and two Cargo jobs.

| Metric | Official Rust | Allocator package |
| --- | ---: | ---: |
| Cold build CPU | 148.50 s | 178.63 s |
| Edit CPU | 46.32 s | 47.28 s |
| Revert CPU | 45.26 s | 44.88 s |
| Target after edit | 102.18 MiB | 81.59 MiB |
| Stripped executable | 1,375,360 B | 1,377,040 B |
| Median runtime CPU | 1.128 s | 1.125 s |

The 30 paired runtime outputs matched. The candidate/official runtime CPU upper estimate was 1.00985. The allocator gain over the base package leaves a cold-build CPU gap against official Rust. The direct benchmark is under `target/compiler-rewrite/cpu-allocator-official-oxipng`.

## Reproduction

Use the [CPU and disk collector](cpu-benchmark.py) after preparing the [pinned workloads and runtime inputs](CPU-BENCHMARK.md#reproduction). Each output directory must be new.

```sh
build="$PWD/target/compiler-rewrite"
official="$HOME/.rustup/toolchains/1.100.0-x86_64-unknown-linux-gnu/bin"
control="$build/toolchains/control/bin"
frust="$build/toolchains/frust-0ef1bc386-native-opt1/bin"
common=(--official "$official" --control "$control" --frust "$frust"
        --jobs 4 --noops 7 --history-edits 4 --fresh --runtime-all)
release_profiles=(off16-nonincremental off16-incremental thin16-nonincremental
                  thin16-incremental)

python3 qualification/cpu-benchmark.py "${common[@]}" --workloads oxipng \
  --profiles dev dev-nonincremental release "${release_profiles[@]}" \
  fat16-nonincremental fat16-incremental --cpus 2-5 --clean --runtime 30 \
  --output "$build/cpu-disk-oxipng-reproduction"
python3 qualification/cpu-benchmark.py "${common[@]}" --workloads ripgrep \
  --profiles dev dev-nonincremental "${release_profiles[@]}" fat16-incremental \
  --cpus 6-9 --clean --runtime 30 --output "$build/cpu-disk-ripgrep-reproduction"
python3 qualification/cpu-benchmark.py "${common[@]}" --workloads fd \
  --profiles dev "${release_profiles[@]}" fat16-incremental \
  --cpus 10-13 --runtime 60 --output "$build/cpu-disk-fd-reproduction"
```

[The report generator](report-cpu-disk.py) takes the three output directories and emits detailed Markdown. Its `--html` mode produces the interactive T3 chart. Raw JSONL, exact tool identities and command logs for this run are under `target/compiler-rewrite/cpu-disk-{oxipng,ripgrep,fd}`. The separate LLVM replays and their limitations are described in [CPU cost of post-LTO machine code generation](LLVM-PARTITION-CPU.md).

Use [the profile runtime comparison](compare-profile-runtime.py) with a completed workload's `--lane` to run its three incremental LTO profiles against one another on the same core. For the allocator trial, prepare a candidate with [the allocator script](prepare-allocator-toolchain.sh) and give the collector its `bin` directory with `--candidate`, selecting `--toolchains frust candidate` or `--toolchains official candidate`.
