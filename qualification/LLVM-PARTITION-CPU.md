# CPU cost of post-LTO machine code generation

The instrumented, code-changing ripgrep build with 16 incremental CGUs spent 51.58 wall seconds in `LLVM_fatlto` out of 54.63 seconds in the fRust compiler invocation. Final native linking took 0.09 seconds. Timings with instrumentation are separate from the benchmark runs. Running the pinned `llc` on the actual optimized ripgrep bitcode took 12.84 CPU seconds. X86 instruction selection accounted for 3.84 CPU seconds and greedy register allocation for 1.21 CPU seconds in its pass report.

The prototype now splits and canonicalizes each optimized module in one LLVM process. It prunes unused prototypes, sorts declarations while preserving definition order, and hashes the resulting bitcode. A previously verified partition can reuse the object only when that exact input digest matches under one fixed LLVM and codegen command. `llc` verifies each miss before the object enters the experiment's cache. The seed and edited modules both go through full LTO before the experiment starts.

## Fusion of splitting and normalization

On the original ripgrep proxy input, a split followed by a separate `opt` invocation for each partition took 5.22 CPU seconds on the edited warm path. The fused splitter took 3.41 CPU seconds, with 14 hits in each case and identical linked relocatable objects. This comparison includes the split, normalization, digest checks, object emission for misses and partial link. It excludes whole-program optimization.

## Actual optimized modules and application linking

`cargo rustc -- --emit=llvm-bc,link` provided the optimized modules from code-changing incremental builds. The probe replaced the regular LTO object in the captured native link command and ran each resulting executable. Ripgrep printed the changed match count. fd rejected a colon where the unedited program accepted it. The warm and fresh split objects matched byte for byte for both applications.

| Application | Stock monolithic emission CPU | Fresh split CPU | Warm split CPU | Warm hits | Stock stripped bytes | Split stripped bytes |
| --- | ---: | ---: | ---: | ---: | ---: | ---: |
| ripgrep | 12.84 s | 13.99 s | 3.44 s | 14 / 16 | 4,152,400 | 4,146,640 |
| fd | 18.92 s | 21.46 s | 4.45 s | 15 / 16 | 4,303,792 | 4,299,760 |

CPU values include user and system time. The machine-only runs used logical CPUs 6–13. The stock measurement used `llc -O3 -relocation-model=pic` with pass timing. The split measurements include partitioning and partial relocation. Cold and miss costs matter because the extra splitting is paid before an object can hit.

On the ripgrep edit, warm split CPU was 8.27 seconds with four partitions and 4.66 seconds with eight, with two and six hits respectively. The initial 16-partition pass took 3.73 seconds. Moving verification to first emission brought the 16-partition warm sample to 3.44 seconds and preserved the relocatable object.

## Executable quality

Thirty alternating CPU-time pairs per command on logical CPU 14 produced identical outputs between the stock and partitioned executables.

| Command | CPU median candidate / stock | One-sided 95% upper estimate |
| --- | ---: | ---: |
| ripgrep literal | 1.001 | 1.004 |
| ripgrep regex | 1.002 | 1.011 |
| fd directory walk | 0.994 | 1.009 |

The regex upper estimate exceeds 1.01. Compiler integration and the full performance gate remain open. These results characterize post-LTO object emission and the resulting application binaries. They provide no end-to-end compile speedup for a packaged compiler, since rustc still emitted the original object in the capture builds.

## Reproduction

Build [the LLVM pass plugin](llvm/sort-declarations.cpp) against the pinned LLVM. Pass the two optimized bitcode files to [the reuse probe](llvm/partition-reuse.py) through `--seed-module` and `--edited-module`. Use `--legacy-split` for the separate split and normalization path. Use a new output directory for each run. The probe emits JSONL with CPU and wall time, split CPU, partition hits and the relocatable object digest. It fails if the edited warm and fresh objects disagree.

The exact link arguments, IR, stdout checks and raw CPU samples are under `target/compiler-rewrite/cpu-partition`.
