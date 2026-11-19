# CPU build and executable benchmarks

Measured on 19 November 2026 on Linux x86-64 with an Intel Core Ultra 7 255H. The three toolchains use Rust 1.100.0. **Official** is the unwrapped rustup-installed compiler and Cargo. **Control** is the unpatched source-built stage-2 package. **fRust** is the patched compiler package with an optimized build of its patched Cargo. The first pilot used debug Cargo by mistake. It is preserved separately and excluded from these tables.

The official and control commands use their own `cargo` and `rustc`. fRust uses `fcargo` and its sibling `frustc` with the experimental object cache and RELR flags unset. Each run uses the same source snapshot, locked dependencies, and six Cargo jobs. Ripgrep builds use logical CPUs 0–5. fd and oxipng use CPUs 6–13. Compare toolchains within each workload and profile. The host was shared with other work.

CPU time is the sum of user and system time for the command and its waited descendants, returned by Linux `wait4`. Wall time is monotonic elapsed time. The main build and runtime samples run without compiler timing instrumentation. Peak RSS is the maximum process RSS. Allocated build bytes come from `du -sk`. Size comparisons use copies stripped with `strip --strip-unneeded`.

A cold seed starts with an empty target. The no-op uses the seeded target with untouched sources. The edit changes program behavior, and the revert restores the baseline behavior. A fresh edited build uses a second empty target with the edit in place. Each build checks the resulting program. Ripgrep changes its reported match count, fd changes the outcome of a colon search, and oxipng changes the compiled version response. The report distinguishes the oxipng version edit from a change to its compressor.

The dev and project release rows use each project's own Cargo configuration. Fat-LTO rows fix opt level 3, debug 0 and strip none. The defaults row leaves incrementality and the CGU count open. The matched data fixes the stated settings on all three compilers.

Each cold, edit, revert and fresh cell is one observation. No-op cells are medians of seven. Runtime cells are medians of twelve, following a warmup. These are characterization measurements. They leave the frozen multi-pair release gate open.

## Build time

Each cell shows **CPU seconds / wall seconds**.

### ripgrep

| Profile | Operation | Official | Control | fRust |
| --- | --- | ---: | ---: | ---: |
| Dev | Cold | 37.001 / 10.636 | 46.921 / 14.267 | 50.411 / 17.231 |
| Dev | No-op | 0.046 / 0.045 | 0.051 / 0.051 | 0.052 / 0.052 |
| Dev | Semantic edit | 1.050 / 0.741 | 1.106 / 0.817 | 1.118 / 0.827 |
| Dev | Revert | 1.039 / 0.724 | 1.066 / 0.812 | 1.089 / 0.799 |
| Project release | Cold | 109.738 / 24.187 | 123.395 / 27.080 | 123.774 / 27.465 |
| Project release | No-op | 0.044 / 0.044 | 0.050 / 0.050 | 0.051 / 0.051 |
| Project release | Semantic edit | 27.714 / 7.274 | 29.722 / 8.039 | 30.169 / 8.288 |
| Project release | Revert | 28.693 / 7.686 | 31.923 / 8.792 | 32.027 / 8.857 |
| Fat LTO defaults | Cold | 89.460 / 48.524 | 102.736 / 54.721 | 101.509 / 56.549 |
| Fat LTO defaults | No-op | 0.045 / 0.044 | 0.051 / 0.051 | 0.052 / 0.051 |
| Fat LTO defaults | Semantic edit | 45.633 / 39.043 | 51.386 / 43.885 | 51.406 / 44.196 |
| Fat LTO defaults | Revert | 35.327 / 29.254 | 42.836 / 35.725 | 44.661 / 37.466 |
| Fat LTO, 16 CGUs, no incrementality | Cold | 97.648 / 57.178 | 118.617 / 71.234 | 106.305 / 60.577 |
| Fat LTO, 16 CGUs, no incrementality | No-op | 0.044 / 0.044 | 0.048 / 0.048 | 0.048 / 0.048 |
| Fat LTO, 16 CGUs, no incrementality | Semantic edit | 46.205 / 39.342 | 51.531 / 44.316 | 51.963 / 44.464 |
| Fat LTO, 16 CGUs, no incrementality | Revert | 44.023 / 37.224 | 59.636 / 52.135 | 64.023 / 56.911 |
| Fat LTO, 1 CGU, incremental | Cold | 90.117 / 58.899 | 107.881 / 67.568 | 105.067 / 65.112 |
| Fat LTO, 1 CGU, incremental | No-op | 0.046 / 0.046 | 0.052 / 0.053 | 0.053 / 0.053 |
| Fat LTO, 1 CGU, incremental | Semantic edit | 47.459 / 47.524 | 55.275 / 55.200 | 47.774 / 47.751 |
| Fat LTO, 1 CGU, incremental | Revert | 49.143 / 49.171 | 52.515 / 52.573 | 56.438 / 56.668 |
| Fat LTO, 16 CGUs, incremental | Cold | 69.898 / 34.814 | 90.861 / 45.128 | 75.939 / 35.151 |
| Fat LTO, 16 CGUs, incremental | No-op | 0.027 / 0.027 | 0.031 / 0.031 | 0.032 / 0.031 |
| Fat LTO, 16 CGUs, incremental | Semantic edit | 35.138 / 28.357 | 28.340 / 23.169 | 27.150 / 21.846 |
| Fat LTO, 16 CGUs, incremental | Fresh edited | 76.966 / 38.026 | 91.004 / 45.760 | 86.481 / 41.712 |
| Fat LTO, 16 CGUs, incremental | Revert | 25.811 / 20.496 | 37.659 / 32.051 | 41.560 / 34.721 |

### fd

| Profile | Operation | Official | Control | fRust |
| --- | --- | ---: | ---: | ---: |
| Dev | Cold | 148.204 / 37.560 | 209.473 / 51.628 | 163.253 / 40.341 |
| Dev | No-op | 0.038 / 0.038 | 0.042 / 0.042 | 0.043 / 0.042 |
| Dev | Semantic edit | 0.512 / 0.347 | 0.518 / 0.374 | 0.519 / 0.377 |
| Dev | Revert | 0.511 / 0.347 | 0.516 / 0.377 | 0.508 / 0.364 |
| Project release | Cold | 234.794 / 63.312 | 280.469 / 78.706 | 240.664 / 64.709 |
| Project release | No-op | 0.080 / 0.081 | 0.088 / 0.089 | 0.088 / 0.087 |
| Project release | Semantic edit | 18.072 / 18.083 | 22.907 / 22.868 | 25.483 / 25.429 |
| Project release | Revert | 18.531 / 18.511 | 21.504 / 21.465 | 22.021 / 21.986 |
| Fat LTO defaults | Cold | 206.548 / 51.044 | 179.391 / 48.792 | 208.968 / 58.667 |
| Fat LTO defaults | No-op | 0.037 / 0.037 | 0.041 / 0.042 | 0.041 / 0.041 |
| Fat LTO defaults | Semantic edit | 23.756 / 23.690 | 20.705 / 20.639 | 24.019 / 23.952 |
| Fat LTO defaults | Revert | 19.313 / 19.255 | 28.786 / 28.693 | 43.943 / 43.853 |
| Fat LTO, 16 CGUs, incremental | Cold | 180.629 / 46.637 | 211.437 / 53.056 | 198.633 / 50.944 |
| Fat LTO, 16 CGUs, incremental | No-op | 0.036 / 0.036 | 0.039 / 0.040 | 0.040 / 0.040 |
| Fat LTO, 16 CGUs, incremental | Semantic edit | 22.211 / 20.251 | 23.402 / 21.264 | 25.857 / 23.751 |
| Fat LTO, 16 CGUs, incremental | Fresh edited | 168.555 / 44.167 | 170.108 / 46.052 | 169.102 / 45.811 |
| Fat LTO, 16 CGUs, incremental | Revert | 20.527 / 18.744 | 23.274 / 21.225 | 23.209 / 21.155 |

### oxipng

| Profile | Operation | Official | Control | fRust |
| --- | --- | ---: | ---: | ---: |
| Dev | Cold | 56.548 / 11.005 | 64.353 / 12.213 | 64.930 / 12.265 |
| Dev | No-op | 0.033 / 0.033 | 0.038 / 0.038 | 0.038 / 0.038 |
| Dev | Semantic edit | 2.540 / 0.780 | 2.708 / 0.871 | 2.622 / 0.825 |
| Dev | Revert | 2.537 / 0.784 | 2.718 / 0.867 | 2.704 / 0.858 |
| Project release | Cold | 27.444 / 13.748 | 33.764 / 16.208 | 37.183 / 16.407 |
| Project release | No-op | 0.028 / 0.027 | 0.031 / 0.031 | 0.031 / 0.031 |
| Project release | Semantic edit | 8.569 / 7.989 | 9.230 / 8.668 | 9.424 / 8.773 |
| Project release | Revert | 8.257 / 7.708 | 8.940 / 8.374 | 9.669 / 8.957 |
| Fat LTO defaults | Cold | 42.477 / 18.216 | 34.389 / 15.721 | 35.754 / 16.666 |
| Fat LTO defaults | No-op | 0.033 / 0.033 | 0.038 / 0.038 | 0.037 / 0.037 |
| Fat LTO defaults | Semantic edit | 22.140 / 20.721 | 13.533 / 12.839 | 10.789 / 9.874 |
| Fat LTO defaults | Revert | 16.390 / 15.215 | 9.791 / 9.119 | 9.778 / 9.050 |
| Fat LTO, 16 CGUs, incremental | Cold | 27.292 / 13.683 | 32.243 / 15.587 | 39.024 / 18.451 |
| Fat LTO, 16 CGUs, incremental | No-op | 0.028 / 0.028 | 0.033 / 0.033 | 0.032 / 0.032 |
| Fat LTO, 16 CGUs, incremental | Semantic edit | 9.146 / 8.476 | 9.906 / 9.133 | 10.903 / 10.013 |
| Fat LTO, 16 CGUs, incremental | Fresh edited | 39.269 / 18.467 | 48.855 / 21.402 | 40.783 / 19.656 |
| Fat LTO, 16 CGUs, incremental | Revert | 10.286 / 9.445 | 10.293 / 9.628 | 10.333 / 9.624 |

## Incremental work and artifact parity

Fresh and warm edited builds use the same 16-CGU incremental fat-LTO settings. The percent reduction compares the warm edit with an empty-target build of the edited source on the same toolchain.

| Workload | Toolchain | Fresh edited CPU | Warm edit CPU | CPU reduction | Warm/fresh executable `.text` |
| --- | --- | ---: | ---: | ---: | --- |
| ripgrep | official | 76.966 | 35.138 | 54.3% | Identical |
| ripgrep | control | 91.004 | 28.340 | 68.9% | Identical |
| ripgrep | frust | 86.481 | 27.150 | 68.6% | Identical |
| fd | official | 168.555 | 22.211 | 86.8% | Identical |
| fd | control | 170.108 | 23.402 | 86.2% | Identical |
| fd | frust | 169.102 | 25.857 | 84.7% | Identical |
| oxipng | official | 39.269 | 9.146 | 76.7% | Identical |
| oxipng | control | 48.855 | 9.906 | 79.7% | Identical |
| oxipng | frust | 40.783 | 10.903 | 73.3% | Identical |

### Successive ripgrep edits

The first matched ripgrep sample favored fRust over official Rust. Two additional edits reused each toolchain's target from the fresh-edited row and changed the match count offset from 1 to 2, then from 2 to 3. The toolchain order reversed between runs. Each rebuild compiled only the `rg` executable and printed the new count.

| Match count offset | Official CPU / wall seconds | Control CPU / wall seconds | fRust CPU / wall seconds |
| --- | ---: | ---: | ---: |
| 2 | 48.661 / 41.835 | 51.751 / 44.275 | 51.584 / 44.254 |
| 3 | 46.543 / 39.742 | 51.434 / 43.923 | 51.562 / 44.129 |

fRust used 6.0% and 10.8% more CPU than official Rust on these two edits and stayed within 0.4% of the source-built control. The initial result does not establish a consistent advantage over official Rust. Raw measurements are in `target/compiler-rewrite/logs/cpu-ripgrep-repeat.log`.

## Edited build resources

Each cell shows **peak process RSS MiB / allocated target MiB / stripped executable MiB**.

### ripgrep

| Profile | Official | Control | fRust |
| --- | ---: | ---: | ---: |
| Dev | 293.63 / 361.50 / 9.76 | 261.93 / 319.50 / 9.76 | 270.65 / 319.50 / 9.76 |
| Project release | 444.06 / 186.43 / 5.06 | 435.91 / 144.44 / 5.06 | 442.67 / 144.44 / 5.06 |
| Fat LTO defaults | 602.59 / 136.49 / 4.24 | 547.45 / 95.52 / 4.24 | 552.93 / 172.74 / 3.96 |
| Fat LTO, 16 CGUs, no incrementality | 602.78 / 137.51 / 4.25 | 558.69 / 96.54 / 4.25 | 551.14 / 96.54 / 4.25 |
| Fat LTO, 1 CGU, incremental | 522.86 / 212.79 / 3.84 | 482.09 / 167.70 / 3.84 | 495.94 / 167.70 / 3.84 |
| Fat LTO, 16 CGUs, incremental | 572.23 / 219.45 / 3.96 | 536.67 / 174.37 / 3.96 | 549.29 / 174.37 / 3.96 |

### fd

| Profile | Official | Control | fRust |
| --- | ---: | ---: | ---: |
| Dev | 262.46 / 368.75 / 12.76 | 240.78 / 364.99 / 12.77 | 246.96 / 364.98 / 12.77 |
| Project release | 502.69 / 263.07 / 3.96 | 474.61 / 263.09 / 3.96 | 484.13 / 263.09 / 3.96 |
| Fat LTO defaults | 503.05 / 321.10 / 3.96 | 474.82 / 271.36 / 3.96 | 484.30 / 271.36 / 3.96 |
| Fat LTO, 16 CGUs, incremental | 568.48 / 358.21 / 4.10 | 526.15 / 304.61 / 4.10 | 535.40 / 304.62 / 4.10 |

### oxipng

| Profile | Official | Control | fRust |
| --- | ---: | ---: | ---: |
| Dev | 264.69 / 230.86 / 2.02 | 260.67 / 212.32 / 2.02 | 271.46 / 212.34 / 2.02 |
| Project release | 327.58 / 56.14 / 1.32 | 271.82 / 56.15 / 1.33 | 283.21 / 79.86 / 1.32 |
| Fat LTO defaults | 331.61 / 74.60 / 1.32 | 273.02 / 57.24 / 1.33 | 285.37 / 80.96 / 1.32 |
| Fat LTO, 16 CGUs, incremental | 328.03 / 102.16 / 1.31 | 274.39 / 81.58 / 1.31 | 286.53 / 81.58 / 1.31 |

## Runtime

The runtime commands search a 32 MiB corpus sixteen times with ripgrep, walk 12,800 files with fd, and optimize eight PNGs with oxipng. All runtime output digests must agree. Each application uses one worker for this comparison.

Each cell shows median **CPU seconds / wall seconds**.

| Workload | Profile | Official | Control | fRust |
| --- | --- | ---: | ---: | ---: |
| ripgrep | Project release | 0.1238 / 0.1243 | 0.1239 / 0.1243 | 0.1239 / 0.1244 |
| ripgrep | Fat LTO, 16 CGUs, incremental | 0.1242 / 0.1252 | 0.1244 / 0.1276 | 0.1251 / 0.1277 |
| fd | Project release | 0.0099 / 0.0079 | 0.0100 / 0.0079 | 0.0099 / 0.0080 |
| fd | Fat LTO, 16 CGUs, incremental | 0.0096 / 0.0077 | 0.0099 / 0.0078 | 0.0099 / 0.0079 |
| oxipng | Project release | 0.2291 / 0.2294 | 0.2296 / 0.2302 | 0.2284 / 0.2287 |
| oxipng | Fat LTO, 16 CGUs, incremental | 0.2747 / 0.2751 | 0.2802 / 0.2806 | 0.2782 / 0.2786 |

All 294 no-op samples executed zero compiler units. All 216 runtime samples matched within their workload and profile. The matrix contains 651 raw measurement rows across 42 workload, profile and toolchain combinations.

## Automatic fat-LTO settings

The default-profile edit measurements are mixed against official Rust. fRust used 51.41 vs 45.63 CPU seconds on ripgrep, 24.02 vs 23.76 on fd, and 10.79 vs 22.14 on oxipng. The automatic settings apply only where the project leaves those settings open.

The ripgrep unit graphs confirmed fat LTO, incrementality and 16 CGUs for the root executable with both the automatic and explicit settings. Some dependency units had different profiles. To check the difference under a separate traced setup, two logical CPUs and two Cargo jobs ran the same three edits with both settings.

| Edit | Automatic CPU seconds | Explicit CPU seconds |
| --- | ---: | ---: |
| 1 | 134.148 | 129.961 |
| 2 | 134.125 | 126.633 |
| 3 | 133.182 | 126.923 |

The median paired CPU ratio was 1.049. This traced lane used different CPU cores and job settings from the main matrix. Its logs and unit graphs are under `target/compiler-rewrite/cpu-default-profile-diagnostic` and `target/compiler-rewrite/cpu-ripgrep1`.

## Tracing and the next measured optimization

Kernel `perf` counters were unavailable on this host. Compiler phase timers and pinned LLVM's user-plus-system pass timers located the expensive work in full LTO. Native linking was under a tenth of a second in the fRust ripgrep trace. X86 instruction selection and register allocation consumed substantial CPU inside LTO's machine emission stage.

[The post-LTO CPU investigation](LLVM-PARTITION-CPU.md) records the optimization tested next. Fusing partition splitting and normalization avoided repeated LLVM process, parse and serialization work. Exact normalized partition reuse from actual optimized modules preserved warm and fresh output object bytes and produced executable ripgrep and fd builds. The revised machine emission path used 3.44 CPU seconds for ripgrep and 4.45 for fd. Monolithic emission used 12.84 and 18.92 respectively. Whole-program optimization still runs. The CPU reduction is scoped to machine emission. Compiler integration, cold and miss budgets, and application runtime qualification remain open.

## Reproduction

Prepare the pinned workload snapshots and runtime corpus under `target/compiler-rewrite`, using the lockfile digests in [cpu-benchmark.py](cpu-benchmark.py). Use a new output directory for each run and replace the CPU lists if the host topology differs.

```sh
build="$PWD/target/compiler-rewrite"
official="$HOME/.rustup/toolchains/1.100.0-x86_64-unknown-linux-gnu/bin"
control="$build/toolchains/control/bin"
frust="$build/toolchains/frust-0ef1bc386-native-opt1/bin"

python3 qualification/cpu-benchmark.py --official "$official" --control "$control" \
  --frust "$frust" --output "$build/cpu-ripgrep-reproduction" \
  --workloads ripgrep --profiles fat16-incremental fat-default dev release \
  fat1-incremental fat16-nonincremental --cpus 0-5 --jobs 6 \
  --trace --fresh --noops 7 --runtime 12

python3 qualification/cpu-benchmark.py --official "$official" --control "$control" \
  --frust "$frust" --output "$build/cpu-apps-reproduction" \
  --workloads fd oxipng --profiles fat16-incremental fat-default release dev \
  --cpus 6-13 --jobs 6 --fresh --noops 7 --runtime 12
```

Raw CPU, user time, system time, wall, RSS, sizes, digests, commands and toolchain identities for the reported run are under `target/compiler-rewrite/cpu-ripgrep1` and `target/compiler-rewrite/cpu-native-apps1`.
