# Full-LTO rebuild after a code-changing edit

Measured on Linux x86-64 on 2026-11-19. The ripgrep source is the locked tree `e89fff89ac9af12e8d4ce9d5fd07beb408ca730f`. The edit changes the text stats expression from `stats.matches()` to `stats.matches().saturating_add(1)`. The harness checks that input `rust\n` reports `1 matches` before the edit and `2 matches` after it. It also records a different executable digest. All builds use opt level 3, fat LTO, incremental compilation, debug 0 and no strip. Unless noted, the codegen-unit count is one.

The source snapshot and build outputs were isolated from the locked tree and from the other compiler work in progress. These are characterization runs on a shared development machine. The frozen paired qualification remains the release gate.

## Compiler host

The stage-2 control and official Rust 1.100.0 use the same LLVM shared library, SHA-256 `517f2e2d3762ccd6fe742c1b2fc720f447777c2589c99f58e7d12bfacf7b908c`. Substituting the official LLVM library would give the control identical bytes.

| Host compiler | Cold seed | Edited wall time | Edited max RSS |
| --- | ---: | ---: | ---: |
| Source-built stage-2 control | 28.25 s | 21.13 s | 521,528 KiB |
| Control with jemalloc preload | 25.74 s | 23.08 s | 483,068 KiB |
| Official Rust 1.100.0 | 27.24 s | 27.72 s | 535,692 KiB |

The allocator substitution produced identical seed and edited executables to the control. Edited wall time increased.

## LLVM pipeline probes

These runs used `cargo rustc` with `-Z time-passes`, CPU affinity 0–5, and copies of one unmodified incremental target. The two stock runs bracketed the probes. The options in these runs were applied to the edited executable crate only. Any follow-up qualification must seed a target with the same effective compiler options it will use for the edit.

| Variant | Edited wall time | LLVM fat LTO | Edited max RSS | Executable bytes |
| --- | ---: | ---: | ---: | ---: |
| Control before probes | 25.97 s | 18.29 s | 498,744 KiB | 4,688,376 |
| Mimalloc preload | 27.63 s | 16.52 s | 516,860 KiB | 4,688,376 |
| Module inliner | 32.29 s | 23.42 s | 466,112 KiB | 4,843,472 |
| Fast instruction selection | 27.26 s | 19.49 s | 434,624 KiB | 4,817,112 |
| Inlining threshold 75 | 23.43 s | 16.04 s | 414,404 KiB | 4,436,944 |
| Control after probes | 29.01 s | 17.72 s | 498,308 KiB | 4,688,376 |

Both stock builds and the mimalloc build produced byte-identical executables. The change to the inlining threshold produced a different executable. The difference between the two control wall times limits the precision of the comparison.

The raw timing and artifact records are under `target/compiler-rewrite/perf-semantic1`.

## Incremental fat-LTO profile

This series built a separate clean seed for each configuration and used the same options for its edited rebuild. The builds used `-C save-temps` to retain the restricted bitcode for the partition experiment. Compare the rows within this series.

| Variant | Cold seed | Edited wall time | Edited LLVM fat LTO | Edited max RSS |
| --- | ---: | ---: | ---: | ---: |
| One CGU | 44.01 s | 31.85 s | 24.09 s | 525,560 KiB |
| One CGU, retain LLVM analyses | 43.00 s | 30.77 s | 20.55 s | 745,576 KiB |
| Sixteen CGUs | 42.18 s | 24.39 s | 22.16 s | 568,248 KiB |

Sixteen CGUs took 23.4% less wall time on this edited rebuild. The stripped binary was 4,153,280 bytes versus 4,025,168 bytes for one CGU, a 3.2% increase. The edit's reported match count and the corpus command output matched. These samples establish a build-time and executable-size tradeoff on the measured workloads.

The measured configuration is now fCargo's default for eligible native fat-LTO builds. Cargo uses the ordinary release profile settings and keeps explicit codegen-unit and incremental settings. The named profile below the `support/profiles` directory records the configuration used during the experiment.

A second code-changing edit used the locked fd tree. The edit also treats colon as a path separator, which changes a search in a fixture containing `with:colon` from success to the existing diagnostic. Both sides used the named profile through packaged fCargo. The reference overrides just the CGU count to one.

| CGUs | Cold seed | Edited wall time | Edited LLVM fat LTO | Edited stripped bytes |
| --- | ---: | ---: | ---: | ---: |
| One | 67.73 s | 26.58 s | 25.39 s | 4,298,992 |
| Sixteen | 65.50 s | 25.33 s | 23.96 s | 4,298,992 |

The edited fd wall time decreased by 4.7%. The wide gap between the two workload gains limits what can be inferred about other crates.

The packaged fCargo selected frustc and passed `-C lto=fat`, `-C codegen-units=16` and `-C incremental` on the native archive fixture. LLVM fat LTO ran on the seed, edited and fresh builds. Edited and fresh builds matched executable `.text` and stdout.

The runtime check made 12 alternating pairs per literal and regex command. Each process searched 16 repetitions of the 32 MiB file with one thread. All stdout digests matched. Sixteen CGUs had paired median runtime ratios of 1.001 and 1.000. Lowering the inlining threshold shrank the stripped binary by 6.7%, with ratios of 0.996 and 1.024. That regex regression rules out adopting the smaller threshold based on these samples.

## Automatic default versus prior default

A follow-up Linux comparison used packaged fCargo, the ordinary release profile with fat LTO, and the same ripgrep source and semantic edit. The control applied Cargo's prior release incremental setting of false. Both builds confirmed the edited match count and recorded a positive full-LTO phase.

| Native release settings | Cold seed | Edited wall time | Edited stripped bytes |
| --- | ---: | ---: | ---: |
| Prior defaults | 42.76 s | 27.60 s | 4,438,248 |
| Automatic incremental fat LTO | 38.71 s | 25.92 s | 4,152,432 |

The edit took 6.1% less wall time and produced a 6.4% smaller stripped executable in this one comparison. The earlier 23.4% result uses a one-CGU incremental reference. These comparisons answer different questions.

## Post-LTO object partition feasibility

An offline proxy ran the pinned LLVM `lto<O3>` pipeline on the seed and edited after-restriction bitcode, split each optimized module into 16 symbol-based partitions, and emitted objects with `llc`. The edited program's unsplit object emission took 9.47 s. Splitting took 2.38 s, emitting all partitions took 11.37 s in series, and the partial relink took 0.03 s. Whole-program optimization took 12.42 s in the edited proxy.

Fourteen object pairs were already identical, yet none of the raw partition bitcode pairs matched. Running the existing `strip-dead-prototypes` pass on each partition recovered seven exact bitcode matches. Sorting the remaining function declarations by symbol name recovered 14 exact bitcode matches. Definition order was preserved. The measured reuse prototype uses SHA-256 of canonical bitcode under one fixed LLVM and codegen invocation. It verifies the canonical inputs and compares the entire warm relocatable object with a fresh split build. For the edited module, normalization, machine emission and partial relinking took 11.02 s fresh and 2.64 s with reuse. The two relocatable objects were byte-identical. On the same CPU set, splitting separately took 2.30 s and monolithic emission took 13.79 s. The 12.42 s whole-program optimization remains required. This measures an offline backend, with application build gains still unverified.

The fixture exercises the changed-IR path and reuses machine code only after whole-program optimization. Application linking, runtime, resource usage, target-specific compiler options, and persistent cache integrity still require qualification before compiler integration.

### Reproduce the partition stage

Compile `llvm/sort-declarations.cpp` as an opt pass plugin with the pinned LLVM headers and shared library. Run `llvm-split -j=16 --preserve-locals=false` on each optimized module. Pass the two output prefixes to `llvm/partition-reuse.py` with the LLVM binary directory, pinned `rust-lld`, the plugin, and an empty output path. The command records seed, warm and fresh emission times and exits unsuccessfully if the resulting objects differ.
