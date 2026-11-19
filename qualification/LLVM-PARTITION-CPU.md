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

The regex upper estimate exceeds 1.01. Compiler integration and the full performance gate remain open. The capture builds still emitted the original rustc object. The measured speedup applies to the replacement emission stage.

A follow-up bucket experiment computed each symbol's default partition once. All canonical bitcode partitions matched the original. Over four alternating CPU pairs per workload, median times were 0.945 times the existing splitter on ripgrep and 0.972 on fd. Individual fd ratios ranged from 0.810 to 1.524. These results leave the upstream splitter in use.

A compiler invocation requesting only LLVM bitcode printed an `LLVM_fatlto` timing but produced a 6.3 KiB module for the native-archive fixture. Requesting bitcode and link output produced a 1.4 MiB module. The compiler integration needs the post-optimization object-emission seam with ordinary executable link outputs still selected.

## Avoiding partition writes and repeat work

The splitter now hashes canonical bitcode in memory and writes materialized bitcode for misses. It records the ordered digests in a small manifest. The comparison uses the same semantic edit, seed objects, splitter, full bitcode keys and linker. Eight alternating CPU pairs per application produced the same relocatable object as the fresh split in every run.

| Application | Warm hits | Bitcode retained before | Bitcode retained after | CPU with all writes | CPU with miss writes |
| --- | ---: | ---: | ---: | ---: | ---: |
| ripgrep | 14/16 | 24.30 MB | 3.14 MB | 7.83 s | 7.70 s |
| fd | 15/16 | 22.78 MB | 1.38 MB | 5.83 s | 5.95 s |

Warm bitcode volume fell by 87% and 94%. The median paired CPU ratios were 0.990 and 1.002. The splitter still serializes canonical bitcode for every partition to derive exact keys. The byte column covers the intermediate partition bitcode. Object sizes are reported separately in the raw samples.

On byte-identical replay inputs, the prototype can hash the entire optimized module and reuse the previously merged object. It hard-links the immutable object within the isolated run, without allocating a second copy. Eight alternating repeats per application tested both the exact seed module and exact edited module. Ripgrep used 0.040 CPU seconds on the exact shortcut versus 11.052 seconds on an all-hit split. fd used 0.033 versus 10.552 seconds. Each exact and split result matched its original object digest. These runs used the host's two low-power cores and their times are comparable within each pair. Whole-program optimization is outside this replay, and the measured shortcut requires exact optimized bytes.

## Direct linking and function order

The ELF replay preserves function and data sections and passes the partition objects directly to the captured native linker. A variant orders function sections from the seed executable's symbols, or from the original seed object for a stripped executable. The recipe uses the normal captured link command and its native libraries. The edit assertions and timed application output digests matched.

| Command | Ordered median CPU ratio | Upper 95% estimate | Repeat median | Repeat upper 95% |
| --- | ---: | ---: | ---: | ---: |
| ripgrep literal | 0.993 | 1.001 | 0.992 | 0.997 |
| ripgrep regex | 0.991 | 1.005 | 0.999 | 1.012 |
| fd directory walk | 0.980 | 0.984 | 0.986 | 0.989 |

Each command used 30 pairs after four warmups in each run. The ripgrep regex repeat exceeded the 1% upper bound. The ordered executables measured 4,146,128 stripped bytes for ripgrep and 4,298,960 for fd. The original executables measured 4,152,400 and 4,303,792 bytes. A separate checked build of the sectioned warm and fresh objects passed full executable byte parity under the direct ordered link for both applications. This screen covers linked ELF replay executables. Qualifying the default still requires integration with rustc's actual target machine and output selection, cold and mixed-miss build budgets, broader runtime coverage and native verification.

## Reproduction

Build [the LLVM pass plugin](llvm/sort-declarations.cpp) against the pinned LLVM. Pass the two optimized bitcode files to [the reuse probe](llvm/partition-reuse.py) through `--seed-module` and `--edited-module`. Use `--legacy-split` for the separate split and normalization path. Use a new output directory for each run. The probe emits JSONL with CPU and wall time, split CPU, partition hits and the relocatable object digest. It fails if the edited warm and fresh objects disagree.

Use `--compare-warm 8` for paired hit-write measurements and `--compare-exact 8` for paired exact-input bypass measurements. [The link layout replay](llvm/link-layout.py) consumes the LTO object, captured arguments, optimized bitcode partitions, and seed and edited executables from a fixture. The detailed inputs are under `target/compiler-rewrite/cpu-partition`. The new replay logs are under `target/compiler-rewrite/partition-disk`.
