# Benchmark methodology and results

No fRust compiler change has passed the frozen Linux budgets. Nothing here is a qualified faster-rebuild result. The 20 November close-out, with each gate marked passed or failed, is in `qualification/GATE-REPORT.md`. Earlier candidate logs and hashes are in `qualification/CANDIDATE.md`. The raw logs live in a local, git-ignored build directory and are not in the repository, so a clone cannot recheck these numbers.

## Conditions

- Host: Linux x86-64. The CPU model, kernel and linker versions are not recorded in the qualification files. macOS was not measured.
- Dates: the candidate samples in the table below were taken on 2026-11-19, after the protocol was frozen that day (`qualification/PROTOCOL.md`). The 20 November full paired judges are the interface-hash cutoff and the prelink-span compiler, recorded after that table. The same day's x86-64-v3 preflight is a three-workload runtime and size check, not a paired build-time judge. The same day's outliner screen is one ripgrep stripped-size build, not a paired judge. The same day's loop-fusion check is a bitcode comparison, not a paired judge. The same day's Attributor screen is a ripgrep release build that crashed in LLVM, not a paired judge. The same day's NewGVN check is a bitcode comparison, not a paired judge. The same day's GVN-sink check is a bitcode comparison, not a paired judge. The 21 November fat-LTO simplification-gap check is a bitcode comparison, not a paired judge. The same day's joint-candidate check did not build a new compiler. The same day's cache-and-size preflight measured the span caches and `.rodata` duplicates and did not build a compiler. The same day's full-LTO preflight without RELR did not build a compiler. The same day's four-budget scan of the saved judges did not package a compiler. The same day's MIR-inlining preflight built ripgrep with the pass forced on and did not package a compiler. The same day's dataflow const-prop preflight forced that MIR pass on one ripgrep build and did not package a compiler. The same day's MIR opt level 3 measurement ran the inliner and limited const-prop together, collected leaf, disk, runtime, and stripped-size samples, and did not package a compiler. The same day's follow-up did not start the 630-pair cohort: the runtime geomean is already 0.99079798, and moving the span caches out of the trace does not change those binaries. The same day's joint preflight was not started: hotbudget, the fastest judged stack, has runtime geomean 0.97299293, and its ripgrep file grew. The same day's six-switch backend preflight was measured: leaf geomean 0.99252570, disk 1.00071044, runtime upper 0.99910136, and rustfmt grew from 3835192 to 3835544. The same day's four-budget section census did not build a compiler: deleting every `.data.rel.ro` byte still leaves a stripped-size geomean of 0.95941447, and hotbudget's runtime geomean 0.97299293 is already above 0.97. The same day's spanout1 compiler was built and judged: the span caches sit beside the compiler prefix, ext-TSP and SelectOptimize are baked while `llvm_args` stays empty, the 630-pair cohort finished at 2026-11-21T06:35:46Z, edited-release geomean 0.24268311 and disk 1.00010149 passed, and runtime upper 0.99755978 and stripped-size geomean 0.99716103 failed. The same day's module-inliner preflight passed `-enable-module-inliner` only on the fRust side of that compiler and did not package a prefix: runtime upper 1.01338880 and stripped-size geomean 1.04715783, with every file over stock. The same day's loop-flatten preflight passed `-enable-loop-flatten` only on the fRust side and did not package a prefix: the three binaries matched the control, the runtime upper was 1.00569215, and the stripped-size geomean stayed 1.00000000. The same day's unroll-and-jam preflight passed `-enable-unroll-and-jam` only on the fRust side and did not package a prefix: stripped sizes stayed at stock, and the runtime upper was 1.00500825. The same day's partial-inlining preflight passed `-enable-partial-inlining` only on the fRust side and did not package a prefix: runtime upper 0.99607085, stripped-size geomean 1.00397687, and every file over stock. The same day's function-merging check compiled one file with `frust-0ef1bc386-spanout1` and did not run a workload preflight: opt-level 3 already merges identical functions, and `-enable-merge-functions` does not change the object when the driver is named `rustc`. The same day's LICM-cap compiler was built as `frust-0ef1bc386-licmcap1` and preflighted with empty `llvm_args`: disk passed at 1.00010149, the runtime upper was 0.99615678, and the stripped-size geomean was 0.99720509 with every file under stock. The 630-pair cohort was not started. The same day's share-generics screen built one ripgrep with `-Z share-generics=on` on that compiler and did not package a prefix: stripped size grew to 4054736/4024992 (1.00738983). The same day's full-LTO search did not preflight a compiler: 31 saved reports have no runtime upper at or under 0.97, and the licmcap median product 0.97304357 would have to fall to 0.89800584 before that compiler's measured upper gap could reach 0.97.
- Stock: the unpatched stage-2 compiler built from the same recipe. Official Rust 1.100.0 is recorded separately in `qualification/BASELINES.md` because its binaries are PGO builds.
- Toolchains: the default package `toolchains/frust` has a compiler, standard library and rustdoc identical to stock and a patched Cargo (`qualification/COMPAT.md`). Each candidate is a separate package whose compiler carries the candidate change.
- Workloads: ripgrep 15.2.0, syn 3.0.10 and regex 1.13.2 are the primary cohort, and fd 10.5.2 is a holdout. The release profile is opt-level 3, fat LTO, one codegen unit, incremental, debug 0, no strip.
- Edits: the frozen leaf and wide edits in `qualification/edits/`. The ripgrep leaf edit adds an unused local. syn and regex are libraries, so their builds have no fat-LTO step.
- Budgets (`crates/frust-core/src/gates.rs`): leaf geometric mean and its upper bound at most 0.80, holdout median below 1, cold median at most 1.05, no-op overhead at most max(5% of stock, 20 ms), tail p95 at most 1.05, peak RSS at most 1.10, target plus cache bytes at most 1.20, runtime per workload at most 1.01 and aggregate at most 0.97, stripped size no larger than stock and aggregate at most 0.97. Medians need at least 30 pairs and tail estimates at least 60.

## Results

Rows marked single sample are one build each, not gate results.

| Candidate | Evidence | Outcome |
| --- | --- | --- |
| Fat-LTO pre-link at opt-level 0 plus x86 machine outliner | Single sample: ripgrep leaf 16.74s against a 19.14s control median (0.875). | Peak RSS 2.11 times the control. Dropped. Gate not run. |
| Fat-LTO pre-link at opt-level 1 | Single sample per primary workload. Leaf ratios: ripgrep 0.951, syn 1.016, regex 1.000, geometric mean 0.989. | Dropped. Gate not run. |
| `--icf=safe` at link time | Single sample: ripgrep leaf 19.12s against 19.14s (0.999). The binary was byte-identical to the control. | Dropped. Gate not run. |
| Pre-optimization fat-LTO cache (`frust-0ef1bc386-preopt1`) | Full paired gate: 60 leaf and 60 wide pairs for each primary workload, 30 cold and 30 no-op pairs for each of four workloads, 30 fd holdout pairs. | Failed four gates. Dropped. |
| Two-thread fat-LTO codegen split (`frust-0ef1bc386-par1`) | Single paired sample: ripgrep leaf 6.37s against 19.37s. fd holdout 21.19s against 15.90s, peak RSS 1.62 times stock. | Gate not run. Dropped. |
| Ordered fat-LTO stream cache (`frust-0ef1bc386-fatkey1`) | Full paired gate, same pair counts as the pre-optimization cache. | Failed five gates. Dropped. |
| Fat-LTO location splice (`frust-0ef1bc386-span1`) | Full paired gate, same pair counts. | Failed four gates. Not qualified. |
| Post-LTO object reuse plus RELR (`frust-0ef1bc386-cgcache1`, `frust-cg1`) | Single samples, see below. | Kept as opt-in research, not qualified. Gate not run. |
| Interface-hash cutoff (`frust-0ef1bc386-cutoff1`) | Full paired judge on 2026-11-20. | Build-time gates passed. Runtime aggregate and stripped size failed. Not kept. |
| Prelink-span reuse (`frust-0ef1bc386-prelinkspan1`) | Full paired judge on 2026-11-20. | Leaf, holdout, cold, noop, tail, and memory passed. Disk, runtime aggregate, and stripped size failed. Not kept. |
| x86-64-v3 plus the hotfreq16 flags, on `frust-0ef1bc386-prelinkspan1` | 30 runtime pairs per workload on 2026-11-20. User flags, not a new compiler. | Runtime geomean 0.98290005. Stripped size under 0.97 only with opt-in RELR. Not kept. Gate not run. |
| Machine outliner plus `-C force-unwind-tables=no`, on `frust-0ef1bc386-prelinkspan1` | One ripgrep release build on 2026-11-20. No RELR. | Stripped 4010848/4024992 (0.99648596). Not kept. Oxipng, rustfmt, and the gate were not run. |
| Loop fusion (`-enable-loopfusion` on the packaged `opt`) | Bitcode comparison on 2026-11-20. Not a user `rustc` flag. | memchr `default<O3>` and ripgrep `lto<O3>` matched with the switch off. Not kept. Gate not run. |
| Attributor (`-attributor-enable=full`) on `frust-0ef1bc386-prelinkspan1` | One ripgrep release build on 2026-11-20. No RELR and no `-C target-cpu`. | `frustc` SIGSEGV while optimizing regex-automata 0.4.15. No binary. Not kept. Gate not run. |
| NewGVN (`-enable-newgvn` on the packaged `opt`) | Bitcode comparison on 2026-11-20. Not a user `rustc` flag. | Ripgrep pre-LTO grew by 9856 bytes. `find_impl` unchanged. An earlier final-crate flag sample already missed runtime and size. Not kept. |
| GVN sink (`-enable-gvn-sink` on the packaged `opt`) | Bitcode comparison on 2026-11-20. The pass is not in `lto<O3>`. | Frozen memchr `find_impl` stays at 695 instructions. Not kept. Gate not run. |
| Fat-LTO simplification gap (O3 passes absent from `lto<O3>`) | Bitcode comparison on 2026-11-21. Not added to rustc. | Ripgrep instruction ratio 0.99040271. `find_impl` keeps its vector loop. Not kept. Gate not run. |
| Joint runtime and size candidate | Decision on 2026-11-21 from the judged stacks and the oxipng `-o 4` profile. | hotbudget runtime upper 0.98418770, size geomean 1.03517556. No compiler packaged. Fresh gate not run. |
| Span-cache relocation plus `.rodata` dedup | Preflight on 2026-11-21. Cache bytes and stripped section census. Not compiled in. | Pessimistic ripgrep disk ratio 1.10520802. Optimistic `.rodata` size geomean 0.98749874. Not kept. Fresh gate not run. |
| Full-LTO reloc elimination without RELR | Preflight on 2026-11-21 from the stripped controls and the static-relocation judge. Not compiled in. | Optimistic size geomean 0.93815212. Measured runtime upper 0.99106385 on `ET_EXEC`. Not kept. Fresh gate not run. |
| Four-budget full-LTO package | Scan on 2026-11-21 of 23 saved judges. Not compiled in. | No report has runtime upper and stripped-size geomean both at or under 0.97. Prelink-span leaf 0.38807512 / 0.38955465, disk 1.25765371. Not kept. Fresh gate not run. |
| MIR inlining on the incremental profile | One ripgrep release build on 2026-11-21, `-Z inline-mir=yes`, frozen profile. Not compiled in. | Stripped 4030752/4024992 (1.00143106). Thirty-pair median 0.99684879, interquartile range crosses 1. Not kept. Fresh gate not run. |
| Dataflow const-prop on the incremental profile | One ripgrep release build on 2026-11-21, `-Z mir-enable-passes=+DataflowConstProp`, frozen profile. The MIR inliner stayed off. Not compiled in. | Stripped 4015920/4024992 (0.99774608). Thirty-pair median 1.00402123, interquartile range crosses 1. `find_impl` unchanged. Not kept. Fresh gate not run. |
| MIR opt level 3 on the incremental profile | Leaf, disk, runtime, and stripped size on 2026-11-21, `-Z mir-opt-level=3` on `frust-0ef1bc386-prelinkspan1`. Not compiled in. | Leaf geomean 0.39330222. Disk 1.20340838. Runtime upper 0.99794438. Oxipng 1338800/1338440. `frust-gates` exited 1. Not kept. |
| Span cache outside the MIR opt level 3 trace | Cache census on 2026-11-21. Not compiled in. The 630-pair cohort was not started. | Subtracting 26050560 cache bytes leaves ripgrep disk 1.05533956. Runtime geomean stays 0.99079798. Not kept. |
| Joint full-LTO preflight | Decision on 2026-11-21. No new compiler was built. | hotbudget geomean 0.97299293 and upper 0.98418770. Ripgrep 4030408/4024992. `frust-gates` exited 1. The 630-pair cohort was not started. |
| Six backend switches on `frust-0ef1bc386-prelinkspan1` | Leaf, disk, runtime, and stripped size on 2026-11-21. One shim, not a new prefix. | Leaf geomean 0.99252570. Disk 1.00071044. Runtime upper 0.99910136. Rustfmt 3835544/3835192. `frust-gates` exited 1. The 630-pair cohort was not started. |
| Same-candidate four-budget compiler | Section census and saved-judge scan on 2026-11-21. Not compiled in. | `.data.rel.ro` deletion would leave geomean 0.95941447, under 0.97; the bytes stay. hotbudget runtime geomean 0.97299293. No report passes all four. The 630-pair cohort was not started. |
| Span caches beside the prefix, plus baked ext-TSP and SelectOptimize (`frust-0ef1bc386-spanout1`) | Full paired judge on 2026-11-21. 630 edit pairs, four traces, three runtime workloads. | Edited-release geomean 0.24268311. Disk 1.00010149. Runtime upper 0.99755978. Size geomean 0.99716103, every file under stock. `frust-gates` exited 1. Not kept. |
| Module inliner (`-enable-module-inliner`) on `frust-0ef1bc386-spanout1` | Runtime and stripped size on 2026-11-21. User flag, not a new prefix. | Runtime upper 1.01338880. Size geomean 1.04715783. Every file over stock. `frust-gates` exited 1. Not kept. |
| Loop flatten (`-enable-loop-flatten`) on `frust-0ef1bc386-spanout1` | Runtime and stripped size on 2026-11-21. User flag, not a new prefix. | Binaries matched the control. Runtime upper 1.00569215. Size geomean 1.00000000. `frust-gates` exited 1. Not kept. |
| Unroll-and-jam (`-enable-unroll-and-jam`) on `frust-0ef1bc386-spanout1` | Runtime and stripped size on 2026-11-21. User flag, not a new prefix. | Stripped sizes matched stock. Runtime upper 1.00500825. Size geomean 1.00000000. `frust-gates` exited 1. Not kept. |
| Partial inlining (`-enable-partial-inlining`) on `frust-0ef1bc386-spanout1` | Runtime and stripped size on 2026-11-21. User flag, not a new prefix. | Runtime upper 0.99607085. Size geomean 1.00397687. Every file over stock. `frust-gates` exited 1. Not kept. |
| Function merging (`-enable-merge-functions`) | One-file check on 2026-11-21. Not a workload preflight. | Opt-level 3 already merges the two copies. The LLVM flag is byte-identical on the `rustc` hardlink. Not a new candidate. |
| LICM MemorySSA caps at 10000 (`frust-0ef1bc386-licmcap1`) | Disk, runtime, and stripped size on 2026-11-21. Baked, empty `llvm_args`. | Disk 1.00010149. Runtime upper 0.99615678. Size geomean 0.99720509, every file under stock. `frust-gates` exited 1. Not kept. |
| Share generics (`-Z share-generics=on`) on `frust-0ef1bc386-licmcap1` | One ripgrep release build on 2026-11-21. Not a paired judge. | Stripped 4054736/4024992 (1.00738983), over stock. `frust-gates` was not run. Not kept. |
| No untried full-LTO change | Scan on 2026-11-21 of 31 saved reports and the default-off fat-LTO switches. Not compiled in. | No runtime upper is at or under 0.97. licmcap stays the best empty-`llvm_args` stack at upper 0.99615678 and size geomean 0.99720509. The 630-pair cohort was not started. |
| Post-link BOLT after fat LTO | Runtime and stripped size on 2026-11-21. `--use-old-text`, no huge pages, 4096-byte text alignment. Not a new prefix. | Runtime upper 1.00621331. Ripgrep grew to 4110536/4024992. Size geomean 1.020932. `frust-gates` exited 1. The 630-pair cohort was not started. |
| Sink common instructions (`-sink-common-insts`) | Runtime and stripped size on 2026-11-21. User flag on top of the licmcap layout flags. Not a new prefix. | Runtime upper 1.00902087. Ripgrep grew to 4082336/4024992. Size geomean 1.02965759. `frust-gates` exited 1. The 630-pair cohort was not started. |
| Knowledge retention (`-enable-knowledge-retention`) | Runtime and stripped size on 2026-11-21. User flag on top of the licmcap layout flags. Not a new prefix. | Runtime upper 1.00079480. Every file under stock. Size geomean 0.99834120. `frust-gates` exited 1. The 630-pair cohort was not started. |

### Interface-hash cutoff, 20 November

`frust-gates` wrote `target/compiler-rewrite/logs/cutoff-gates.json` (sha256 `7bf8d360b8d72e684f516189ba8ea84237c44aa07f75112f113d08ef13713a7a`) and exited 1. Edited-release geomean 0.68822331 (upper 0.68958244), holdout median 0.21986888, cold median 1.00599739, no-op overhead -0.00064504 s, tail within 1.05, peak RSS 1.06854079, and disk 1.09510395 passed. Runtime-workloads passed only because ripgrep, oxipng, and the rustfmt port were byte-identical to stock. Runtime aggregate failed at geomean 0.99844853, upper 1.00384281. Stripped size failed at geomean 1.00000000. Those build-time passes are this compiler's. The cutoff is not the default toolchain. The figures are repeated in `qualification/GATE-REPORT.md`.

### Prelink-span reuse, 20 November

`frust-gates` wrote `target/compiler-rewrite/logs/prelinkspan-gate/gates.json` (sha256 `9ef47b85c6c72379cb74f4aecfacb16f91c3d67e21cdf591bdfa4d63a2896b74`) and exited 1. A second run matched those bytes. Samples sha256 `dfc4ba56b31a970df4025c692e446669faa4ee695813b4b7a0e0c535208c6bbd`. Edited-release geomean 0.38807512 (upper 0.38955465), holdout median 0.08624574, cold median 1.02498955, no-op overhead 0.00084712 s, tail within 1.05, and peak RSS 1.03380829 passed. Disk failed at 1.25765371 because the ripgrep trace target was 221265920 bytes against 175935488. Runtime-workloads passed because ripgrep, oxipng, and the rustfmt port were byte-identical to stock (4024992, 1338440, and 3835192). Runtime aggregate failed at geomean 1.00478501, upper 1.01877364. Stripped size failed at geomean 1.00000000. Leaf medians were ripgrep 0.34329093, syn 0.31461318, and regex 0.54113815. This prefix stays off the default toolchain. The figures are repeated in `qualification/GATE-REPORT.md`.

### x86-64-v3 codegen preflight, 20 November

The same prelink-span driver rebuilt ripgrep, oxipng, and the rustfmt port with `-C target-cpu=x86-64-v3`, ext-TSP, `-hot-callsite-rel-freq=16`, and `-Wl,-z,pack-relative-relocs` on the fRust side. Oxipng's C archive used Clang 21 and no extra `CFLAGS`. Median ratios were ripgrep 0.99123229, oxipng 0.94419073, and rustfmt 1.01459543. Their product is 0.94957238 and the geomean is 0.98290005. Stripped sizes were 3797080/4024992, 1333624/1338440, and 3656480/3835192, geomean 0.96412151, with `DT_RELR` in each fRust binary. Section arithmetic that restores the stock reloc section puts all three files above stock. RELR stays opt-in. No prefix was packaged and `frust-gates` was not run. The figures are repeated in `qualification/GATE-REPORT.md`.

### Outliner plus unwind-table screen, 20 November

The same driver built ripgrep once with `-enable-machine-outliner=always` and `-C force-unwind-tables=no`. RELR was left off. `strip --strip-unneeded` left 4010848 bytes against 4024992 (0.99648596). `.text` fell by 153856 bytes and the unwind sections grew by 139708, so the file saved 14144 bytes. `.rela.dyn` stayed 289056. The binary still has 5228 outlined functions and the corpus stdout matches the stock digest. A 0.97 size geomean would require the other two stripped ratios to multiply to at most 0.915891. Oxipng and rustfmt were not built, and `frust-gates` was not run. The figures are repeated in `qualification/GATE-REPORT.md`.

### Loop fusion bitcode check, 20 November

`PipelineTuningOptions::LoopFusion` stays false in rustc's pass builder, which does not read `--enable-loopfusion`. The packaged LLVM 23.1.5 `opt` does. The memchr prelink module through `default<O3>` was 103976 bytes either way. The ripgrep pre-LTO module through `lto<O3>` was 5211360 bytes either way. The memchr module contains one `find_impl`. The ripgrep module contains none. No stage-2 rebuild was started and `frust-gates` was not run. The figures are repeated in `qualification/GATE-REPORT.md`.

### Attributor size screen, 20 November

`-attributor-enable=full` adds `attributor` and `attributor-cgscc` to `default<O3>` and adds neither to `lto<O3>`. The ripgrep release build used that flag and the frozen profile, without RELR or `-C target-cpu`. `frustc` SIGSEGV'd while optimizing regex-automata 0.4.15. The backtrace cycles through `Attributor::isAssumedDead` until rustc stops at its maximum depth. The same library compiled without the flag. No ripgrep binary was produced, and `frust-gates` was not run. The figures are repeated in `qualification/GATE-REPORT.md`.

### NewGVN bitcode check, 20 November

`-enable-newgvn` defaults off. Before `-passes`, the packaged LLVM 23.1.5 `opt` prints `newgvn` in `lto<O3>` where it otherwise prints `gvn<>`. The memchr prelink module through `default<O3>` grew from 103976 to 104004 bytes. Its 43 functions match after SSA names, block labels, and debug metadata are stripped, and `Finder::find_impl` is unchanged. The ripgrep pre-LTO module through `lto<O3>` grew from 5211360 to 5221216 bytes. Of 2415 functions, 463 bodies differ and none is `find_impl`. An earlier final-crate flag sample had already missed the runtime aggregate (geomean 1.00150149, upper 1.00678247) and stripped size (geomean 1.00427080). This comparison was not judged again. No stage-2 rebuild was started. The figures are repeated in `qualification/GATE-REPORT.md`.

### GVN-sink bitcode check, 20 November

`-enable-gvn-sink` inserts `gvn-sink` into `default<O3>` and does not insert it into `lto<O3>`. On the frozen memchr prelink module the switch moves the output from 103976 to 103952 bytes and leaves `find_impl` at 695 instructions. From unoptimized memchr 2.8.3 the AVX2 `find_impl` stays at 902 instructions, and the lines that differ are not the vector loop. No stage-2 rebuild was started and `frust-gates` was not run. The figures are repeated in `qualification/GATE-REPORT.md`.

### Extra vectorizer passes, 20 November

`-extra-vectorizer-passes` inserts `extra-vector-passes` into `lto<O3>` and adds `early-cse` after `slp-vectorizer`. On the frozen memchr prelink module the switch leaves all 43 bodies matching and leaves `find_impl` at 580 instructions. The ripgrep pre-LTO module shrinks from 5211360 to 5210928 bytes. Of 2415 functions, 27 bodies differ and none is `find_impl`. Reaching a 0.97 ratio on this module would remove 156341 bytes. An earlier span1 sample of the same flag already missed both budgets: runtime geomean 0.99336408, upper 0.99959463, stripped-size geomean 0.99993278. This comparison was not judged again. The figures are repeated in `qualification/GATE-REPORT.md`.

### Fat-LTO simplification gap, 21 November

`lto<O3>` does not run `correlated-propagation`, `reassociate`, `libcalls-shrinkwrap`, `adce`, `loop-rotate`, `loop-idiom`, `simple-loop-unswitch`, or `loop-interchange`. Appending them after `lto<O3>` shrinks the ripgrep pre-LTO module from 5211360 to 5176044 bytes and from 227981 to 225793 instruction lines (ratio 0.99040271). `find_impl` stays at 676 instruction lines and keeps its `<16 x i8>` compare. A second `default<O3>` grows that function to 696 lines. Carrying the whole 35316-byte bitcode delta into stripped ripgrep, with oxipng and rustfmt left at stock, leaves a three-file geomean of 0.99706668. No stage-2 rebuild was started and `frust-gates` was not run. The figures are repeated in `qualification/GATE-REPORT.md`.

### Joint candidate, 21 November

No new compiler was packaged. A rerun of shipped `frust-gates` on the prelink-span samples matched `target/compiler-rewrite/logs/prelinkspan-gate/gates.json` (sha256 `9ef47b85c6c72379cb74f4aecfacb16f91c3d67e21cdf591bdfa4d63a2896b74`, exit 1): disk 1.25765371, runtime geomean 1.00478501, runtime upper 1.01877364, stripped-size geomean 1.00000000. hotbudget remains the fastest judged full-LTO stack, at runtime upper 0.98418770 and size geomean 1.03517556. The oxipng `-o 4` samples put 31.8% in `deflate_compress_greedy`, whose matchfinder is the hash chain already covered by the `-march=haswell` median 0.977. RELR stays opt-in. The 630-pair cohort was not started. The figures are repeated in `qualification/GATE-REPORT.md`.

### Span-cache relocation plus rodata dedup, 21 November

The prelink-span ripgrep seed is 202477568 allocated bytes, and its `frust-prelink-span` and `frust-fatspan-cache` directories are 26820608 of that. Subtracting those bytes from the judged ripgrep trace leaves 194445312/175935488 = 1.10520802. syn, regex, and fd are already 1.07875774, 1.06219488, and 1.11444453. On the stripped controls, folding every repeated 32-byte `.rodata` window leaves ratios 0.98419475, 0.99129733, and 0.98701708, geomean 0.98749874. That pair of changes was not compiled in. hotbudget's runtime upper and size geomean stay above 0.97. The figures are repeated in `qualification/GATE-REPORT.md`.

### Full-LTO change without RELR, 21 November

Replacing all 12044, 1236, and 10256 dynamic relocations with 4-byte relative pointers would leave a stripped-size geomean of 0.93815212. The measured static-relocation judge already reached 0.93980369 and missed the runtime upper at 0.99106385, on `ET_EXEC` binaries. Spending the remaining size room at the measured inliner rate projects a runtime upper of 0.98449989. Virtual-function elimination was not built, because it does not rewrite `find_impl` or the libdeflate hash chain. No compiler was packaged and no fresh sample was judged. The figures are repeated in `qualification/GATE-REPORT.md`.

### Four-budget full-LTO package, 21 November

The four remaining budgets are leaf geomean and upper at or under 0.80, disk at or under 1.20, runtime geomean and upper at or under 0.97, and stripped-size geomean at or under 0.97. None of the 23 saved judges has runtime upper and stripped-size geomean both at or under 0.97. The prelink-span compiler passes leaf at 0.38807512 / 0.38955465 and fails disk at 1.25765371, with runtime and stripped size at the stock binaries. The cutoff compiler passes leaf and disk and emits those same stock binaries. No compiler was packaged and no fresh sample was judged. The figures are repeated in `qualification/GATE-REPORT.md`.

### MIR inlining on the incremental profile, 21 November

Opt-level 3 uses MIR opt level 2, which skips the MIR inliner when incremental compilation is on. Forcing `-Z inline-mir=yes` on one frozen-profile ripgrep build left a stripped file of 4030752/4024992 (1.00143106). Thirty pairs against the joint control had median 0.99684879, and the interquartile range crossed 1. The corpus stdout matched. Oxipng and rustfmt were not built, because ripgrep is already over stock. No compiler was packaged and no fresh sample was judged. The figures are repeated in `qualification/GATE-REPORT.md`.

### Dataflow const-prop on the incremental profile, 21 November

`DataflowConstProp` is the MIR pass that starts at opt level 3. It propagates scalars, skips bodies over 100 basic blocks, and tracks at most 100 places. Forcing it with `-Z mir-enable-passes=+DataflowConstProp`, and leaving the MIR inliner off, left stripped ripgrep at 4015920/4024992 (0.99774608). `.text` fell by 8800 bytes. The AVX2 `find_impl` stays 1788 bytes and its instructions match the control once branch targets are relocated. Thirty pairs had median 1.00402123, and the interquartile range crossed 1. A 0.97 size geomean would need the other two stripped ratios to multiply to at most 0.91473474. Oxipng and rustfmt were not built. No compiler was packaged and no fresh sample was judged. The figures are repeated in `qualification/GATE-REPORT.md`.

### MIR opt level 3 on the incremental profile, 21 November

`-Z mir-opt-level=3` turns the MIR inliner on for ordinary bodies and runs `DataflowConstProp` with its 100-block and 100-place limits. It was applied on `frust-0ef1bc386-prelinkspan1` with `RUSTC_BOOTSTRAP=1`, an empty `llvm_args` list, fat LTO, and no RELR. Shipped `frust-gates` wrote `target/compiler-rewrite/logs/mir-level3-gates.json` (sha256 `f5264082dee4f7480b26fccb77efb6bd9b8611033dc801a4bfec6f3496f7495b`) and exited 1. A second run matched those bytes. Samples sha256 `e5854d8d6e8aabe32d998f66002f4b0ca117c3d400085ee33cd30dabaeff2df5`. Edited-release passed at geomean 0.39330222 (upper 0.39545133). Leaf medians were ripgrep 0.31859473, syn 0.34772735, and regex 0.54916361. Disk failed at 1.20340838. Runtime medians were 0.99031274, 1.00189169, and 0.98030719, so the aggregate is geomean 0.99079798 and upper 0.99794438. Stripped sizes were 4021360/4024992, 1338800/1338440, and 3754952/3835192. Oxipng grew by 360 bytes, and the size geomean is 0.99276701. Holdout, cold, noop, and tail were not collected, so those gates are inconclusive. No compiler was packaged. The figures are repeated in `qualification/GATE-REPORT.md`.

### Full cohort not started, 21 November

The MIR opt level 3 runtime geomean is 0.99079798. Shipped tests `spread_sample_upper_bound_is_at_least_the_point` and `zero_variance_upper_bound_matches_the_point` passed, 2 passed and 0 failed, so the upper bound is at least that geomean and cannot be at or under 0.97. The ripgrep seed's span caches are 26050560 bytes. Subtracting them from the judged trace leaves 185671680/175935488 = 1.05533956. That relocation was not compiled in, and it returns the same binaries. Replacing the ripgrep median with the size-neutral `find_impl` edit 0.9807 leaves geomean 0.98758173. The 630-pair cohort was not started. No compiler was packaged. The figures are repeated in `qualification/GATE-REPORT.md`.

### Joint full-LTO preflight not started, 21 November

The compiler that would enter this preflight keeps fat LTO, PIE, unwind tables, and an empty `llvm_args` list, so the prelink-span cache stays eligible. That cache is the measured build-time result under 0.80: edited-release geomean 0.39330222 on the MIR opt level 3 stack. The same compiler would also have to put the runtime upper at or under 0.97 and every stripped file at or under stock. Shipped `spread_sample_upper_bound_is_at_least_the_point` and `zero_variance_upper_bound_matches_the_point` passed again, 2 passed and 0 failed, so the upper is at least the geomean. Shipped `frust-gates` on `target/compiler-rewrite/logs/hotbudget-samples.txt` exited 1 and matched `logs/hotbudget-gates.json` (sha256 `581ff18fbe42609314fb6ba4b2d7ba57548ca03434b6bc787e7dc2229143f225`). Runtime geomean 0.97299293 and upper 0.98418770 are both above 0.97, and stripped size failed because ripgrep grew from 4024992 to 4030408. A 0.97 geomean needs a median product at or under 0.912673. hotbudget's judged product is 0.92114723. No new compiler was built, and the 630-pair cohort was not started. The figures are repeated in `qualification/GATE-REPORT.md`.

### Six backend switches, 21 November

The stack is `frust-0ef1bc386-prelinkspan1` with six `-C llvm-args` switches: `enable-cse-in-irtranslator`, `enable-cse-in-legalizer`, `enable-split-loopiv-heuristic`, `enable-rem-conv`, `x86-early-ifcvt`, and `x86-promote-anyext-load`. In the pinned LLVM 23.1.5 tree the two CSE switches are GlobalISel and default off, `enable-rem-conv` is Hexagon, and `enable-split-loopiv-heuristic` and `x86-promote-anyext-load` already default on. `x86-early-ifcvt` defaults off. A non-empty `llvm_args` list turns the prelink and fat-span caches off. The profile stayed opt-level 3, fat LTO, one codegen unit, incremental, debug 0, and no strip. There was no `-C target-cpu` and no RELR. Shipped `frust-gates` wrote `target/compiler-rewrite/logs/backend6-gates.json` (sha256 `79454bfb8d93b6b42c7cc6e2adc0b68796a2f2e6fe81d5cdd2311f71e1024b45`) and exited 1. A second run matched those bytes. Samples sha256 `13f82e79ff2e54eaacfea94cca46131353a087452250289759bd99077ba866a8`. Edited-release failed at geomean 0.99252570 (upper 0.99655857). Leaf medians were ripgrep 1.00005010, syn 0.99679794, and regex 0.98083601. Disk passed at 1.00071044 and peak RSS passed at 1.04233562. Runtime medians were 0.99275668, 1.00127218, and 0.98529353, so the aggregate is geomean 0.99308602 and upper 0.99910136. Their product is 0.97940113, above the 0.912673 a 0.97 geomean requires. Stripped sizes were 4024272/4024992, 1338312/1338440, and 3835544/3835192. Rustfmt grew by 352 bytes, and the size geomean is 0.99993908. Each binary is `FLAGS_1 NOW PIE` with no `DT_RELR`. Holdout, cold, and no-op were not collected, so those gates are inconclusive. No prefix was packaged. The 630-pair cohort was not started. The figures are repeated in `qualification/GATE-REPORT.md`.

### Four-budget candidate not built, 21 November

The candidate keeps fat LTO, PIE, unwind tables, an empty `llvm_args` list, and RELR off, and it would store the span caches outside the traced Cargo target. Empty `llvm_args` is what keeps the leaf budgets eligible. The six-switch stack missed them at geomean 0.99252570 because its `llvm_args` list was not empty. Subtracting the measured 26050560 cache bytes from the MIR opt level 3 ripgrep trace leaves disk ratio 1.05533956 and does not change the binaries. Fresh `readelf` on the three stock stripped controls (4024992, 1338440, 3835192; `FLAGS_1 NOW PIE`; no `DT_RELR`) gives `.rela.dyn` 289056, 29664, and 246144, and `.data.rel.ro` 212856, 23168, and 196160. Deleting every path-like string (22131, 13370, and 18097 bytes) leaves size geomean 0.99326185. Deleting every `.data.rel.ro` byte leaves product 0.88311812 and geomean 0.95941447, under the 0.912673 product a 0.97 geomean requires. Those bytes stay, because the program addresses them. Deleting `.rela.dyn` leaves geomean 0.94703128, and that cut is RELR or a non-PIE static link. Twenty-six saved reports record both geomeans, and none has a runtime upper and a size geomean both at or under 0.97. hotbudget's runtime geomean is 0.97299293 (`logs/hotbudget-gates.json`, sha256 `581ff18fbe42609314fb6ba4b2d7ba57548ca03434b6bc787e7dc2229143f225`). Shipped `spread_sample_upper_bound_is_at_least_the_point` and `zero_variance_upper_bound_matches_the_point` passed, 2 passed and 0 failed, so the upper is at least that geomean. No compiler was built. The 630-pair cohort was not started. The figures are repeated in `qualification/GATE-REPORT.md`.

### Span-cache relocation plus baked layout, 21 November

`frust-0ef1bc386-spanout1` stores `frust-prelink-span` and `frust-fatspan-cache` beside the compiler prefix, and it bakes `-enable-ext-tsp-block-placement` and `-disable-select-optimize=false` only when the process name is `frustc` and `llvm_args` is empty. The profile stayed opt-level 3, fat LTO, one codegen unit, incremental, debug 0, and no strip. There was no `-C target-cpu` and no RELR. The shipped path check printed `PROVE_OK 2026-11-21T04:14:00Z`: the cache files are `prelink-v2-*-tsp-sel-*` and `span-v2-*-tsp-sel-*` under the prefix, the Cargo target does not contain those directories, and a leading newline on `panic!("span")` hits the prelink cache. `compiler/rustc_codegen_llvm` has `test = false`, so the stage-1 test filter ran 0 library tests and is not the path proof. Stage-2 cargo was not rebuilt (`openssl.pc` was missing); the prefix cargo is the prelinkspan1 content-checksum binary. `rustdoc` and `frustdoc` are the previous prefix's binaries. The cohort does not invoke rustdoc. Shipped `frust-gates` wrote `target/compiler-rewrite/logs/spanout-gate/gates.json` (sha256 `36b7eb27493b49c5d3e0779305daa3b0b85af61bc7a2f4119f1c86fae214e8a8`) and exited 1. A second run matched those bytes. Samples sha256 `19c9d432fa36f20c7d4063fa317554d590d4de0c570245bae22d65f42d4b4592`. The cohort ran from 2026-11-21T04:16:40Z to `JUDGE_EXIT 1` at 2026-11-21T06:35:46Z. Edited-release passed at geomean 0.24268311 (upper 0.24328197). Leaf medians were ripgrep 0.08453531, syn 0.31267826, and regex 0.54073293. Holdout passed at 0.08740022. Cold passed at 0.56759310 because the prefix cache survives an empty scratch. No-op overhead passed at 0.00052144 s. Tail p95 stayed within 1.05. Peak RSS passed at 1.03282158. Disk passed at 1.00010149 (syn 40361984/40357888). Runtime-workloads passed. Paired-ratio medians were 0.99231013, 0.99722426, and 0.99465002, product 0.98426162. Runtime aggregate failed at geomean 0.99472611 and upper 0.99755978. A 0.97 geomean needs a product at or under 0.912673. Stripped sizes were 4012128/4024992, 1334920/1338440, and 3824872/3835192, each under stock, geomean 0.99716103. Each runtime row records `identical` 0. The prefix stays off the default toolchain. The figures are repeated in `qualification/GATE-REPORT.md`.

### Module inliner, 21 November

`-enable-module-inliner` defaults off. In LLVM 23.1.5 it replaces the CGSCC `ModuleInlinerWrapperPass` with `ModuleInlinerPass` in the per-module pipeline and in `buildLTODefaultPipeline`, and both paths take `getInlineParamsFromOptLevel` for the same optimization level. The preflight used `frust-0ef1bc386-spanout1` and passed the flag only on the fRust side as `-C llvm-args=-enable-module-inliner`. A non-empty `llvm_args` list turns the span caches off and skips the baked ext-TSP and SelectOptimize switches. The profile stayed opt-level 3, fat LTO, one codegen unit, incremental, debug 0, and no strip. There was no `-C target-cpu` and no RELR. Ripgrep and oxipng used `qualification/run-runtime.py`. Rustfmt used the existing port: `RUSTC_BOOTSTRAP=1` on both sides, a control `--sysroot` of the spanout1 prefix, and the flag only on the fRust side. The port appends a row when the command exits 0. It does not compare formatted trees. `run-runtime.py` discards stdout. Each runtime row records `identical` 0. The run went from 2026-11-21T06:52:24Z (load 0.01) to `JUDGE_EXIT 1` at 2026-11-21T06:56:00Z, under `nice -n 15` with `CARGO_BUILD_JOBS=4`. Shipped `frust-gates` wrote `target/compiler-rewrite/logs/modinl-preflight/gates.json` (sha256 `0f4df719a2034aa4739d316995bf4ed6df5f7ed4c73f9095713504b2b1713b85`) and exited 1. A second run matched those bytes and that stdout. Samples sha256 `128e4f576e14bb8eaac0f4bc3b1024d35d7e4d7bfe880815a0064371b53f65e4`. Paired-ratio medians were ripgrep 0.99549373, oxipng 1.00128714, and rustfmt 1.02205287. Their product is 1.01875682. The aggregate failed at geomean 1.00621358 and upper 1.01338880. Runtime-workloads failed because the rustfmt upper was 1.0348. Every interquartile range crosses 1. Stripped sizes were 4221664/4024992, 1369408/1338440, and 4103656/3835192. Every file grew. The size geomean is 1.04715783. Leaf, disk, cold, no-op, tail, holdout, and peak memory were not collected, so those gates are inconclusive. No prefix was packaged. The 630-pair cohort was not started. The figures are repeated in `qualification/GATE-REPORT.md`.

### Loop flatten, 21 November

`-enable-loop-flatten` defaults off. `buildLTODefaultPipeline` inserts `LoopFlattenPass` ahead of indvars and the vectorizer when the flag is on and the level is at least O2. The pass flattens a nest only when every induction use has the form `(outer phi * inner trip count) + inner phi`. The preflight used `frust-0ef1bc386-spanout1` and passed `-C llvm-args=-enable-loop-flatten` only on the fRust side. A non-empty `llvm_args` list turns the span caches off and skips the baked ext-TSP and SelectOptimize switches. The profile stayed opt-level 3, fat LTO, one codegen unit, incremental, debug 0, and no strip. There was no `-C target-cpu` and no RELR. Ripgrep and oxipng used `qualification/run-runtime.py`. Rustfmt used the existing port, with `RUSTC_BOOTSTRAP=1` on both sides, a control `--sysroot` of the spanout1 prefix, and the flag only on the fRust side. The run went from 2026-11-21T07:00:55Z (load 0.28) to `JUDGE_EXIT 1` at 2026-11-21T07:04:12Z, under `nice -n 15` with `CARGO_BUILD_JOBS=4`. Each unstripped binary matched the control (`identical` 1). Stripped sizes stayed 4024992, 1338440, and 3835192. Shipped `frust-gates` wrote `target/compiler-rewrite/logs/flatten-preflight/gates.json` (sha256 `4fa4d19cac86ceb442a8a3098393ce87a02d667ab59f792bdb117ab6c57b428d`) and exited 1. A second run matched those bytes and that stdout. Samples sha256 `00e85dcf751f0a67ca7592f2873ae1004400d87c0f685f566e93eee5f9a35ad2`. Paired-ratio medians were 0.99936752, 0.99975070, and 0.99611261, product 0.99523442. Runtime-workloads passed because the binaries match. The aggregate failed at geomean 0.99840894 and upper 1.00569215. Stripped size failed at geomean 1.00000000. Clang 21 had already compiled libdeflate's `deflate_compress.c` to the same 75096-byte object with this flag. No prefix was packaged. The 630-pair cohort was not started. The figures are repeated in `qualification/GATE-REPORT.md`.

### Unroll-and-jam, 21 November

`-enable-unroll-and-jam` defaults off. `addVectorPasses` runs `LoopUnrollAndJamPass` in the fat-LTO post-link pipeline when the flag is on and loop unrolling is enabled. The preflight used `frust-0ef1bc386-spanout1` and passed `-C llvm-args=-enable-unroll-and-jam` only on the fRust side, so the span caches and the baked layout switches stayed off. The profile stayed opt-level 3, fat LTO, one codegen unit, incremental, debug 0, and no strip. There was no `-C target-cpu` and no RELR. The run went from 2026-11-21T07:05:28Z (load 0.86) to `JUDGE_EXIT 1` at 2026-11-21T07:08:46Z, under `nice -n 15` with `CARGO_BUILD_JOBS=4`. Unstripped ripgrep and oxipng differed from the control. Unstripped rustfmt matched. Stripped sizes stayed 4024992, 1338440, and 3835192. Shipped `frust-gates` wrote `target/compiler-rewrite/logs/jam-preflight/gates.json` (sha256 `d736fadec77cb3a955142b6d3e66d927ea7c3630cefd2d2dc2f77338efed8bec`) and exited 1. A second run matched those bytes and that stdout. Samples sha256 `87fef645a89d1bfd7a4310c8345a58a508c371cd1b3164d3fd766a2d9015e143`. Paired-ratio medians were 1.00275003, 0.99857589, and 1.00378027, product 1.00510727. Every interquartile range crosses 1. Runtime-workloads passed. The aggregate failed at geomean 1.00169953 and upper 1.00500825. Stripped size failed at geomean 1.00000000. Clang 21 had already compiled `deflate_compress.c` to the same 75096-byte object with this flag. No prefix was packaged. The 630-pair cohort was not started. The figures are repeated in `qualification/GATE-REPORT.md`.

### Partial inlining, 21 November

`-enable-partial-inlining` defaults off. `buildModuleOptimizationPipeline` runs `PartialInlinerPass` when the flag is on, and fat-LTO pre-link uses that pipeline through `buildPerModuleDefaultPipeline`. The post-link `buildLTODefaultPipeline` does not. The preflight used `frust-0ef1bc386-spanout1` and passed `-C llvm-args=-enable-partial-inlining` only on the fRust side, so the span caches and the baked layout switches stayed off. The profile stayed opt-level 3, fat LTO, one codegen unit, incremental, debug 0, and no strip. There was no `-C target-cpu` and no RELR. The run went from 2026-11-21T07:09:56Z (load 0.68) to `JUDGE_EXIT 1` at 2026-11-21T07:13:14Z, under `nice -n 15` with `CARGO_BUILD_JOBS=4`. Each runtime row records `identical` 0. Shipped `frust-gates` wrote `target/compiler-rewrite/logs/partinl-preflight/gates.json` (sha256 `66d3c1000a83d8ce62323502942fd468f89dbb6ab621c8da34aa218825f044a1`) and exited 1. A second run matched those bytes and that stdout. Samples sha256 `71257c5e13708e152e776aa8e45e4872e9b1fa07893c343e305cd2a22606ef29`. Paired-ratio medians were ripgrep 0.99663049, oxipng 1.00129911, and rustfmt 0.97576234, product 0.97373786. Rustfmt's interquartile range is 0.93607341 to 0.99089685, under 1. Ripgrep and oxipng cross 1. The aggregate failed at geomean 0.99116818 and upper 0.99607085. Runtime-workloads failed because the ripgrep upper was 1.0112. Stripped sizes were 4042072/4024992, 1343240/1338440, and 3850920/3835192. Every file grew. The size geomean is 1.00397687. No prefix was packaged. The 630-pair cohort was not started. The figures are repeated in `qualification/GATE-REPORT.md`.

### Function merging, 21 November

Opt-level 3 already runs LLVM function merging on this host. The x86_64 target uses `MergeFunctions::Aliases`, and `LLVMRustOptimize` writes that choice into `PTO.MergeFunctions` after the pass builder has copied the default-off `enable-merge-functions` flag. A direct `frustc` compile of two `#[inline(never)]` functions with the same body, fat LTO, one codegen unit, and debug 0 keeps only `dup::beta` and strips to 289344 bytes. `-Z merge-functions=disabled` keeps `dup::alpha` and `dup::beta` and strips to 294800 bytes. The `rustc` hardlink does not bake the layout switches. With and without `-C llvm-args=-enable-merge-functions` it emits the same bytes, sha256 `5f2f7b8385f8fcd08541b9cabf52327b4c2f92f85119cde6573b011f0c6153e8`, stripped 290272, and only `dup::beta`. No workload preflight was started. No prefix was packaged. The 630-pair cohort was not started. The figures are repeated in `qualification/GATE-REPORT.md`.

### LICM MemorySSA caps, 21 November

LLVM's LICM stops precise MemorySSA clobber queries after `licm-mssa-optimization-cap` (default 100) and skips promotion after `licm-mssa-max-acc-promotion` (default 250). `PipelineTuningOptions` copies both caps after command-line parsing, and rustc does not overwrite them. `frust-0ef1bc386-licmcap1` bakes `-licm-mssa-optimization-cap=10000` and `-licm-mssa-max-acc-promotion=10000` next to ext-TSP and SelectOptimize, only when the process is named `frustc` and `llvm_args` is empty. The span-cache object token is `tsp-sel-licm`, so a spanout cache is not reused. The stage-2 rebuild exited 0 at 2026-11-21T07:35:56Z. The new driver is `librustc_driver-c56a332a1933ca87.so`, sha256 `de3a9e94523e532f2b2caf508c88a87cf8d7734a34a563e0971399111360c8f4`. `frustc` and `rustc` are one inode. A direct fat-LTO compile of a small loop differed between those two names, and the `frustc` binary printed 385. The preflight left `llvm_args` empty. It ran from 2026-11-21T07:37:07Z (load 1.36) to `JUDGE_EXIT 1` at 2026-11-21T07:45:20Z, under `nice -n 15` with `CARGO_BUILD_JOBS=4`. Shipped `frust-gates` wrote `target/compiler-rewrite/logs/licmcap-preflight/gates.json` (sha256 `237615ff6923337091f6a5bcfac8d9f2792f20e66b3a2b2d832fda8c4e068ef8`) and exited 1. A second run matched those bytes and that stdout. Samples sha256 `d0f27ef1e8d9bab097e58cfbcfa3357ac5c894de70019d171973f054d8d77ef4`. Disk passed at max ratio 1.00010149 (syn 40361984/40357888). Peak memory passed at 1.04296012. Paired-ratio medians were ripgrep 0.99283355, oxipng 0.99421491, and rustfmt 0.98576994, product 0.97304357. Nearest-rank interquartile ranges are ripgrep 0.97937026 to 1.00910989, oxipng 0.98913123 to 1.00062291, and rustfmt 0.98158127 to 0.99586705. Ripgrep and oxipng cross 1. Rustfmt stays under 1. The aggregate failed at geomean 0.99093255 and upper 0.99615678. A 0.97 geomean needs a product at or under 0.912673. Runtime-workloads passed. Stripped sizes were 4012576/4024992, 1334920/1338440, and 3824952/3835192. Each file is under stock. The size geomean is 0.99720509. Each runtime row records `identical` 0. Leaf, holdout, cold, no-op, and tail were not collected. The 630-pair cohort was not started. The prefix stays off the default toolchain. The figures are repeated in `qualification/GATE-REPORT.md`.

### Share generics, 21 November

`-Z share-generics` overrides the opt-level default. Opt-level 3 leaves sharing off. One ripgrep release build on `frust-0ef1bc386-licmcap1` passed `-Z share-generics=on` with `RUSTC_BOOTSTRAP=1`. The `rg` crate line was `frustc` with `-C opt-level=3`, `-C lto=fat`, `-C codegen-units=1`, and no `llvm_args`. A second build reproduced the stripped file. `strip --strip-unneeded` left 4054736 bytes against stock 4024992 (1.00738983) and against the licmcap default 4012576. The file is over stock, so stripped size fails before oxipng and rustfmt are built. The frozen corpus command wrote 423040 bytes, sha256 `118c0e6684d6cc46308cdd44baaa594af0a5cc83d19aa282ef5fcfc48e2e925f`. A two-crate fat-LTO program was byte-identical with sharing on and off (unstripped sha256 `78d98c812acd75db77ba51a335bcd0a796539141e26464010808cbcfd2cf9ca8`, stripped 312768), which does not change the ripgrep result. No runtime pairs were collected. `frust-gates` was not run. The flag was not baked in. The 630-pair cohort was not started. The figures are repeated in `qualification/GATE-REPORT.md`.

### No both-bounds full-LTO change, 21 November

No compiler was preflighted. Thirty-one saved reports carry a runtime or size figure, and none has a runtime upper at or under 0.97. hotbudget is the lowest upper, 0.98418770, with geomean 0.97299293 and size geomean 1.03517556. That geomean is already above 0.97. Shipped `spread_sample_upper_bound_is_at_least_the_point` and `zero_variance_upper_bound_matches_the_point` passed, 2 passed and 0 failed, so the upper is at least the geomean. licmcap remains the best empty-`llvm_args` stack with every stripped file under stock: product 0.97304357, upper 0.99615678, size geomean 0.99720509. Its upper sits 0.00522423 above the geomean, so the product has to reach 0.89800584 before that upper can reach 0.97. A 0.97 geomean by itself needs a product at or under 0.912673. Placing the loopfix ripgrep median 0.97515134 next to the licmcap oxipng and rustfmt medians leaves product 0.95571382. hotbudget's product is 0.92114723. The best medians from different probes multiply to 0.92824213. The licmcap size product is 0.99163869. Adding the measured 14144-byte machine-outliner saving on top of licmcap ripgrep, as if the cuts did not overlap, leaves size geomean 0.99603202. The remaining default-off switches are pre-link outlining, knowledge retention, loop-versioning LICM, GVN hoist, matrix lowering, an Oz-trained ML inliner, and profile or instrumentation passes. Post-link mandatory inlining is already first, speculative devirtualization is off for every LTO phase, and post-link loop interchange was already projected to size geomean 0.99706668 with `find_impl` unchanged. Fat LTO does not recompile `deflate_compress.c`. No prefix was packaged. The 630-pair cohort was not started. The figures are repeated in `qualification/GATE-REPORT.md`.

### Pre-optimization cache gate

The shipped `frust-gates` judged the paired file. Cold (median ratio 1.0050), no-op, tail, peak RSS (maximum 1.0844) and target bytes (maximum 1.1445) passed. The leaf geometric mean was 0.6885 (upper 0.6889), but it comes from one workload. The median paired ratios were ripgrep 0.3316 (6.29s against 19.02s), syn 0.9844 and regex 1.0000. The ripgrep leaf edit adds an unused local that is removed before the fat-LTO merge, so the cache hit. The ripgrep wide edit changes the merged module and ran at 1.0296.

Four gates failed:

- Holdout: the fd median ratio was 1.0308, which is not an improvement.
- Runtime workloads: locked rustfmt 1.9.0 did not build with the packaged sysroot (10 `E0463` errors), so only ripgrep and oxipng ran and three are required.
- Runtime aggregate: geometric mean 0.9891, upper bound 1.1369, against 0.97. The upper bound is driven by the ripgrep command, a process of about 7 ms. The binary that ran first in a pair was slower: the median ratio was 1.335 for odd pairs and 0.750 for even pairs. The paired median was 0.978.
- Stripped size: only two artifacts, so the gate is inconclusive and counts as failed. Their geometric mean was 0.9545.

### Ordered stream-cache gate

`frust-gates` judged `frust-0ef1bc386-fatkey1` on 2026-11-19. The leaf geometric mean was 0.6912 (upper 0.6946). Median paired ratios were ripgrep leaf 0.3319, syn leaf 1.0038, and regex leaf 0.9913. The ripgrep leaf is an unused local that dies before the hash, so the cache hit and the binary matched stock. Cold median 1.0027, no-op overhead 0.0002s, peak RSS 1.0531, and target bytes 1.0506 passed.

Five gates failed:

- Holdout: the fd median of paired ratios was 1.0239. The fd edit changes a line-number constant, so the cache missed and the full fat-LTO pipeline ran.
- Tail: regex leaf p95 was 1.0556. Syn leaf 1.0628, regex wide 1.1415, and ripgrep wide 1.4116 are also above 1.05. The ripgrep wide median ratio was 1.0129.
- Runtime workloads and runtime aggregate: rustfmt 1.9.0 did not build (10 `E0463` errors). Ripgrep and oxipng ran and were byte-identical to stock, with runtime geomean 0.9974.
- Stripped size: two artifacts, both the same size as stock (ripgrep 4024992, oxipng 1338440).

The host load average reached 19 during this gate while other sessions compiled fd and oxipng. These samples were not retimed. The patch is `support/patches/rustc/dropped/0006-frustc-fatlto-stream-cache.patch`.

### Location-splice gate

`frust-gates` judged `frust-0ef1bc386-span1` on 2026-11-19. The leaf geometric mean was 0.6816 (upper 0.6865). Median paired ratios were ripgrep leaf 0.3327 (6.45s against 19.76s), syn leaf 0.9773, and regex leaf 0.9740. The ripgrep leaf and the fd holdout are line-number edits. The splice hit, and fd's paired median was 0.2182 (3.49s against 15.92s). Cold median 0.9967, no-op overhead 0.0009s, peak RSS 1.0271, and target bytes 1.0479 passed. The ripgrep speedup is the unused-local edit dying before fat LTO. It is not a general leaf speedup. Syn and regex are libraries and have no fat-LTO step, so the splice does not run for them.

Four gates failed:

- Tail: syn leaf p95 was 1.6110. Regex wide p95 was 1.0716. The other four primary series were at or under 1.05. Syn's unpaired wall medians were 3.45s and 4.73s; the gate uses the median of paired ratios, 0.9773, because the two sides move in opposite directions across pairs.
- Runtime workloads and runtime aggregate: rustfmt 1.9.0 did not build (44 API errors, no `E0463`). The unpatched stage-2 fails the same `Block.tokens` lookup. Ripgrep and oxipng ran and were byte-identical to stock. Their two-workload runtime geomean was 0.9845 and its upper bound was 1.0364, against 0.97.
- Stripped size: two artifacts, both the same size as stock (ripgrep 4024992, oxipng 1338440).

The patch is `support/patches/rustc/experimental/0007-frustc-fatlto-span-reuse.patch`. It stays applied. This is not a qualified candidate.

### Kept candidate, single samples

All rows below are one sample from `toolchains/frust-cg1`, not the 30-pair gate. Ratios are against the frozen control rows in `qualification/BASELINES.md`.

| Workload | Kind | Wall ratio | Peak RSS ratio |
| --- | --- | --- | --- |
| ripgrep | leaf | 0.655 (12.53s against 19.14s) | 0.903 |
| syn | leaf | 1.026 | 1.017 |
| regex | leaf | 1.030 | 1.023 |
| fd | holdout | 1.014 | 1.040 |
| fd | cold | 0.978 | 1.110 |

The ripgrep leaf hit the object cache because its optimized module did not change. The syn and regex edits change the edited crate's object, so the cache did not help them. The three-leaf geometric mean is about 0.885, above 0.80. The fd cold RSS ratio of 1.110 is above the 1.10 budget. The stripped ripgrep binary was 3742608 bytes against 4024992 (0.930). That build also packed relative relocations. The corpus runtime ratio for ripgrep was 0.991 over 30 runs in one session, on a command of about 6 ms. That is not under 0.97, and oxipng was not rebuilt with this candidate for these samples. The current patch text makes object reuse and RELR opt-in, and `qualification/CANDIDATE.md` does not record the switch values for these samples.

### Flag checks that were not packaged

Stock-compiler characterizations on ripgrep, with oxipng added for the x86-64-v3 scheduling check, are in `qualification/CANDIDATE.md`: `-enable-machine-outliner=always`, `--icf=all`, `-C force-unwind-tables=no`, `-Z tune-cpu` for Arrow Lake and x86-64-v3, and `-hot-cold-split`. None was added to the compiler. None was shown to meet both the 0.97 runtime and stripped-size aggregates. Each is a single build or session, not a paired gate sample.

### Cargo unit cache and no-op overhead

The published results come from the Benchmark workflow (the official comparison described in [CI measurement method](#ci-measurement-method)). The first recorded run is in [Results of the CI comparison](#results-of-the-ci-comparison). Local-host development measurements, taken on one machine with pinned cores, are in `qualification/OPTIMIZED-LINUX.md` and are earlier local measurements, not release claims. A first build with an empty store also pays for writing cache entries.

## CI measurement method

The Benchmark workflow (`.github/workflows/benchmark.yml`) is started by hand and holds two measurements. Neither writes a number into this repository. Numbers are published only from a completed run, by hand, with the run and commit named.

### Official comparison

Official Rust against the fRust toolchain with its Cargo unit cache, on four GitHub-hosted platforms. Linux x86-64 has two, one per shipped build of the toolchain, and each is compared with the official toolchain installed on its own runner:

| Platform | Runner label | Target | fRust build |
| --- | --- | --- | --- |
| `linux-x86_64` | `ubuntu-24.04` | `x86_64-unknown-linux-gnu` | baseline x86-64 |
| `linux-x86_64-v3` | `ubuntu-24.04` | `x86_64-unknown-linux-gnu` | compiler and tools built for x86-64-v3 |
| `macos-arm64` | `macos-latest` | `aarch64-apple-darwin` | the one macOS build |
| `macos-x86_64` | `macos-15-intel` | `x86_64-apple-darwin` | the one macOS build |

The `platforms` input names them `linux`, `linux-v3`, `macos-arm64` and `macos-x86_64`. The v3 build runs only where the runner CPU has every x86-64-v3 extension (`.github/scripts/cpu-supports-v3.sh`). A job on a runner without them runs nothing, says so in a workflow warning and in the run summary, and leaves a record that the report lists in its run notes.

#### Candidate: native rustc PGO on macOS

Two more platform names are accepted and are off by default: `macos-arm64-pgo` (`macos-latest`, `aarch64-apple-darwin`) and `macos-x86_64-pgo` (`macos-15-intel`, `x86_64-apple-darwin`). They measure the macOS toolchain built with profile-guided optimization of `rustc`, against official Rust, in the same way as the plain macOS platforms. Only the toolchain differs: it comes from its own Toolchains call, artifact (`toolchain-frust-pgo-<target>`), cache key and report label, and it is packaged and installed by the same steps. The measurement, the cohorts and the official comparison are unchanged, and the report adds a side-by-side table of `macos-<arch>` and `macos-<arch>-pgo` when both ran in one dispatch. The release recipe is not changed. As of 21 November 2026 this candidate has not been built or measured, so no PGO speed-up is claimed.

`.github/scripts/build-toolchain-pgo.sh` builds it on the runner that will run it, in one build directory:

1. A stage-2 `rustc` and standard library built with `pgo.rustc.generate`. Bootstrap applies the PGO flags to stage 2 only, so stages 0 and 1 are shared with the next build.
2. Training with rustc-perf's collector (`support/tools/pgo-rustc-train.py`), one crate and build profile at a time (`Check`, `Debug`, `Opt`; scenarios `All`), under an 80 minute budget and 25 minutes per unit. The crates are upstream's `RUSTC_PGO_CRATES` plus the application-style crates of `support/patches/build/0001-frust-pgo-training-set.patch`, which are tried first; none belongs to a measured cohort (`qualification/cohorts.lock`), and the script refuses one that does. Raw profiles are merged into one profile as they appear and deleted, because one file per `rustc` process adds up to far more than a macOS runner's disk. `llvm-profdata` is the one in the official `llvm-tools` of Rust 1.100.0 and must report the pinned LLVM version.
3. The instrumented stage-2 outputs are removed and the full plain recipe is built again with `pgo.rustc.use`, verbosely (`-vv`) so that every compiler command and LLVM's profile diagnostics are logged. Clippy, rustfmt, rust-analyzer, rustdoc and Cargo are built by the PGO compiler but have no profile of their own.

`support/tools/pgo-rustc-evidence.py` records and judges the evidence, and the toolchain is packed only if it passes: the instrumented driver has profile counter sections and the final one has none; at least 50 compiler commands carry `-Cprofile-generate` in the first build and `-Cprofile-use=<the merged profile>` in the second, and none carries the other; the training produced at least 50 raw files, none or at most 2% unusable, from at least 4 crates complete in every profile, and the merged profile is an IR profile with at least 20000 functions; LLVM's hash-mismatch diagnostics stay under 5% of the profiled functions and its missing-function diagnostics under 50% (Cargo hides warnings of registry crates, so these count the in-tree compiler crates only); and the profile was produced and used on the same target (the script runs only on a native macOS machine of the target's architecture). The summary is appended to the run summary, and `evidence.json`, `MANIFEST` and the build and training logs are uploaded as `pgo-evidence-frust-pgo-<target>` for 14 days. A cached toolchain produces none of these, and the run summary says so; use `rebuild-toolchains` to regenerate them.

Dispatch for one same-run comparison of the plain and PGO builds on both macOS hosts (no Linux jobs; the PGO builds take about four hours before measurement begins, and the plain macOS jobs wait for them because the compare jobs share one `needs` list):

```
gh workflow run benchmark.yml -f protocol=false \
  -f platforms="macos-arm64 macos-arm64-pgo macos-x86_64 macos-x86_64-pgo" \
  -f cohorts="ripgrep syn regex fd" -f rounds=8 -f noops=5 -f rebuild-toolchains=false
```

To start only the candidates, use `-f platforms="macos-arm64-pgo macos-x86_64-pgo"`. The step budget of a toolchain build is 355 minutes. The script reads these optional environment variables: `FRUST_PGO_TRAIN_BUDGET_MINUTES` (80), `FRUST_PGO_UNIT_TIMEOUT_MINUTES` (25), `FRUST_PGO_MIN_CRATES` (4), `FRUST_PGO_TRAIN_CRATES` (comma separated crate names), `FRUST_PGO_TRAIN_PROFILES` (`Check,Debug,Opt`), `FRUST_PGO_TRAIN_SCENARIOS` (`All`), `FRUST_PGO_MAX_MISMATCH_PERCENT` (5) and `FRUST_PGO_MAX_MISSING_PERCENT` (50).

- Official toolchain: `rustup toolchain install 1.100.0 --profile minimal`, pinned to that exact version. The driver refuses to run unless `rustc -Vv` reports `release: 1.100.0`.
- fRust toolchain: the packaged stage-2 toolchain built natively on a runner of the same kind by the Toolchains workflow, run as shipped through `fcargo` and `frustc`, and installed the way a user installs it (see [Installing the toolchains](#installing-the-toolchains)). The experimental switches are off, so the comparison measures the shipped defaults. The published binaries of official Rust are PGO builds and the fRust recipe is not, which is why the frozen stock budgets use the unpatched stage-2 of the same recipe instead (`qualification/PROTOCOL.md`).
- Sources: the commits and `Cargo.lock` hashes of `qualification/cohorts.lock`, fetched by `qualification/fetch-cohort.sh`, built `--locked --offline`. Each job builds one workload, from ripgrep, syn, regex and fd. Both toolchains build the same tree.
- Profile: the shared release profile of the protocol (opt-level 3, fat LTO, one codegen unit, incremental, no debug info, no strip) on both sides.
- Driver: `qualification/ci-bench.py`, which reuses the `Toolchain` class and helpers of `qualification/run-store-bench.py`. Charts and tables: `qualification/make-charts.py`.

Scenarios, each measured once per round per toolchain:

| Scenario | Starting state |
| --- | --- |
| cold build | Empty target directory. fRust starts with an empty unit cache store. |
| clean rebuild | Target directory removed. The fRust store holds the entries written by that round's cold build. Official Rust has no store, so this is its cold build again. |
| no-op rebuild | Immediate rebuild in the clean rebuild's target. The sample is the median of several builds (`noops` input). |
| edit rebuild | The frozen edit of `qualification/edits/` (leaf and wide for ripgrep, syn and regex, holdout for fd) applied to a copy of the clean rebuild's target, then reversed with the original file times restored, so one edit never makes the next sample rebuild extra files. |

### Installing the toolchains

Each toolchain is installed by its own installer, so that the files are in the state a user gets:

- Official Rust is installed by rustup, as above.
- fRust goes through `frust-install`, which each compare job builds from the repository. The toolchain that the Toolchains workflow built is packed into the release archive format by `.github/scripts/package-release.sh`, the script and arguments of the Release workflow (the archive, its `.sha256`, a manifest with the release id and, on Linux x86-64, the variant). `frust-install install --archive ... --checksum ... --prefix ...` then installs it into a prefix under the home directory, the file system rustup uses for the official toolchain, and the driver runs `current/bin/fcargo` and `current/bin/frustc` of that prefix. The job checks that the installed manifest names the variant it asked for.

This matters beyond fidelity. Whether the compiler maps `librustc_driver` and `libLLVM` text with 2 MiB pages depends on how those files entered the page cache, not on their bytes. Large buffered writes give large folios on Linux file systems that support them (rustup writes in 16 MiB chunks, and `frust-install` rewrites large files with 32 MiB writes after extraction). `tar` writes in 10 KiB chunks and gives small folios, as does faulting an evicted file back in. A toolchain unpacked with `tar` and measured against one installed by rustup would therefore differ in iTLB behaviour, which shows up as a small CPU difference that has nothing to do with the compilers. GitHub runner kernels are recent enough for this to apply.

Two controls follow, both Linux only:

- Every job records the state for both toolchains before the first timed build and after the last: the largest `Rss` and `FilePmdMapped` of the driver and of `libLLVM` seen in `/proc/PID/smaps` while a probe compile runs (`qualification/toolchain-folio-state.py`). They are stored as `folio` records in the raw results, and the report shows the PMD-mapped amounts per job. A toolchain at 0 against one with large mappings is a state difference, not a compiler difference. The probe itself reads the files, so a file that reclaim had already evicted is faulted back in with small folios by the probe, as it would be by the first build.
- The `cache-state` input chooses what the timed builds see. `installed`, the default, leaves both toolchains as their installers left them, which is what users get. `evicted` runs `qualification/page-cache-state.py evict` on the library files of both toolchains before every timed build (after a `sync`, because dirty pages are not dropped), so both start every build from the same cold-cache state and the compilers alone are compared. It is a diagnostic series: its results say how much of a difference comes from the page cache, and they are not what a user sees. On macOS the input has no effect, the job says so in a notice, and the results record `installed`.

### Unit cache store rules

The store is in the cache directory, so a store left by an earlier build would turn a cold fRust build into a warm one. The driver therefore owns it:

- `FRUST_HOME` points into the job's scratch directory for fRust builds. It moves the store on Linux and on macOS, where `XDG_CACHE_HOME` is not read (`docs/configuration.md`). Ambient `FRUST_HOME` and `FRUST_UNIT_CACHE*` values are dropped, and `XDG_CACHE_HOME` is redirected to a scratch directory as well.
- Before every cold fRust build the store is deleted and checked empty. A store that cannot be emptied stops the run.
- After the cold build the store must hold files. An empty store means the cache did not use the isolated path, so the cold and clean series would be wrong, and the run fails.
- A clean rebuild always follows the cold build of the same round, so its store contents come from that round only.
- The official toolchain has no store, and `FRUST_UNIT_CACHE` is `0` for it.
- The unit cache prints one line per decision with `FRUST_UNIT_CACHE_DEBUG=1`. The driver enables it for fRust builds and records the counts, so the report can show hits and misses next to the CPU.

The frozen pair protocol jobs (`qualification/run-pairs.py`, control against fRust on Linux) follow the same rules with `--unit-cache on`: the store is emptied before the series and before every fRust cold sample, and a clean series builds the unmodified tree cold into an empty target with a fresh store before the timed rebuild. The clean series is not a gate input. It and the size of the store around every build are written to a separate JSON lines file. The resource gate still counts target bytes only, and the trace record carries the store bytes beside it.

### Measurement and statistics

- CPU is user plus system seconds of the whole cargo process tree, read from `wait4`. Wall time and peak RSS are recorded beside it.
- A pair is one round. The two toolchains build one after the other on the same runner, and the order alternates between rounds. Each round also resets the store and the target, so rounds are independent. One unmeasured official build runs before the first round, so unpacking the fetched registry sources is not charged to whichever toolchain happens to build first.
- On Linux both sides run under `taskset` on every available CPU with `-j` equal to that count. macOS has no `taskset`, so nothing is pinned there, `-j` is the logical CPU count, and the metadata record says so. macOS runners are virtual machines on shared hosts and their timing is noisier.
- The reported figure per scenario is the median of the per-pair ratios of fRust CPU to official CPU, as a percent, with the number of pairs, the range of the pair ratios and the number of pairs in which fRust was faster. The medians of the two CPU columns are shown for scale. A summary row per scenario is the geometric mean of the per-workload medians.
- A scenario is flagged, in the table and in the chart, when the median is not below 100 percent, when fRust was slower in some pairs, when fewer than three pairs were taken, when fewer pairs were taken than requested, or when a no-op rebuild compiled units. Nothing is dropped because it is unfavourable.
- Builds that exit non-zero are recorded, excluded from the pairs and listed in the report.
- A job starts no round that would end after 285 minutes of measuring, so jobs stay inside the six hour limit of a hosted runner once the setup before the measurement (building and installing the toolchains, fetching the cohort) and the closing probe and upload are added. The default is eight rounds. A run cut short by the budget records a stop line, the report lists it in its run notes and flags the scenarios that have fewer pairs than requested, so a short series is visible as one.

### Recorded environment

Each raw result file starts with a meta record: platform label, runner CPU model, physical and logical core counts (and performance and efficiency cores on Apple silicon), OS, memory, pinning mode, `-j`, both toolchains' `cargo -V` and `rustc -Vv` output, the repository commit, the workflow run, the cohort commit and `Cargo.lock` hash, the profile, the cache state, the toolchain prefixes and the fRust environment. The `folio` records of the page-cache probe follow it. The report lists every distinct host, because runner hardware varies between jobs of one platform, and the two Linux variants run on separate runners that can differ.

### Output

Every `compare` job uploads its raw JSON lines (samples and page-cache records) and cargo logs as `compare-<platform>-<workload>`. The `report` job downloads them all and uploads `benchmark-report`, with `results.md`, `summary.json` and one `chart-<platform>.svg` per platform. Each chart has one horizontal bar per scenario and workload, with 100 percent marked. The report has a table of the page-cache state of both toolchains per job, and where both Linux variants were measured, a table that sets the two side by side, each as a percent of its own official toolchain. The job also writes the table to the run summary and commits nothing.

### Limits

- Shared runners are noisy and their hardware is not chosen. The ratios are within-job, so they do not depend on the runner, but they are valid only for the hardware recorded with them.
- The edit scenarios use the frozen edits, which sit inside the inlining cone of their callers (see below). They are not a general edit workload.
- The official comparison does not use `qualification/time-build.sh`, whose empty-target check uses `find -quit`, which is GNU specific, and which always runs plain `cargo` rather than `fcargo`. The protocol jobs still use it, turn the unit cache on with `FRUST_UNIT_CACHE=1`, and stay on Linux x86-64.
- The runtime and stripped size gates are not part of the official comparison.

### Contention and tool startup

The Benchmark workflow's regex job on each platform also compares overlapping commands against the corresponding fRust 0.5.2 archive. It alternates eight pairs at the same workspace paths and records process-tree CPU, peak process RSS, lock wait messages and the allocated footprint of targets and caches. Linux records write bytes from `wait4` in 512-byte units. Both platforms report the unit-cache write counter separately. The matrix includes dev and release builds, check storms, and cold, warm, restored and edited targets.

The same job captures thirty alternating pairs of version commands for `fcargo`, `frustc`, `fclippy`, `frustdoc`, `frustfmt` and `rust-analyzer`, with output parity checked on every pair. The contention and startup data are attached to the runner artifact separately from the official Rust release comparison.

A separate diagnostic instruments `flock` and `close` in both toolchains. It counts nonblocking conflicts and records the duration of blocking calls after a conflict and of observed lock holds through unlock, conversion or close. Holds without a recorded end appear separately. The trace runs after the CPU and storage samples so the instrumentation overhead stays out of those comparisons.

Overlapping checks of packages with a shared host proc macro have their own paired CPU and lock traces. The graph has no build scripts, so this exercises the units eligible for cooperative reuse in 0.6.0.

The wide-graph check runs two concurrent warm checks of 388 workspace members. It enables cooperative checking in both toolchains and measures paired CPU time and peak process RSS over six rounds.

Dispatch Benchmark with `protocol=false` and `platforms=macos-write-probe` to test write accounting on both macOS runners. The probe uses a private APFS disk image and checks fresh writes, rewrites, clones and idle activity. Its output reports completed writes to a virtual device and the APFS allocation delta for each phase.

The macOS regex jobs also run six paired storage trials on a private APFS image. Each run calibrates the image before measuring and keeps workspace files, targets, caches and Cargo temporary files on it. The report counts completed volume data writes and container metadata writes through the post-build sync. Occupied APFS blocks include shared extents once. CPU and RSS comparisons come from the separate untraced runs.

The Linux regex jobs use a check graph with 97 checksummed dependencies to measure cache accounting writes. Six pairs start with an empty store and six rebuild a clean target from the warmed store. These checks disable incremental compilation.

## Results of the CI comparison

Source: [Actions run 44836473809](https://github.com/Seggys116/frust/actions/runs/44836473809), 2026-11-21, repository commit `92558c5cabcbc3e3ee35edfd62d8b1db9630717f`. Every number below is copied from the report of that run. The report and the raw samples are artifacts of the run (`benchmark-report` and `compare-<platform>-<workload>`). They are not committed to this repository.

Scope: official Rust 1.100.0 (rustc 0ef1bc386, 2026-11-09) against fRust with the unit cache on, release profile, 8 pairs per scenario and workload. The workloads are ripgrep, syn, regex and fd. The platforms are `linux-x86_64` (baseline build), `linux-x86_64-v3`, `macos-arm64` and `macos-x86_64`. Each figure is fRust CPU as a percent of official CPU, the median of the per-pair ratios. Lower is better and 100% is equal. The `all workloads` rows are the geometric mean of the per-workload medians, and their pair count is the total over workloads. The method is in [CI measurement method](#ci-measurement-method).

The candidate tables and single samples earlier in this file, and the local measurements in `qualification/OPTIMIZED-LINUX.md`, are earlier local measurements. They are not release claims. The figures in this section are the ones to cite for a comparison with official Rust.

Runners are shared and their hardware differs from job to job. Within one platform the jobs ran on these CPUs: `linux-x86_64`, AMD EPYC 9V74 and Intel Xeon 6975P-C; `linux-x86_64-v3`, Intel Xeon 6975P-C; `macos-arm64`, Apple M1 (Virtual) and Apple M2 Pro (Virtual); `macos-x86_64`, Intel Core i7-8700B. Each ratio compares two builds taken one after the other on the same runner. The two Linux columns are each a percent of the official toolchain on their own runners, so they are not CPU times to set against each other.

### All workloads, per platform

`linux-x86_64`:

| Scenario | fRust CPU as % of official | Pairs |
| --- | --- | --- |
| cold build | 100.5% | 32 |
| clean rebuild | 45.5% | 32 |
| no-op rebuild | 89.3% | 32 |
| edit rebuild, leaf | 100.1% | 24 |
| edit rebuild, wide | 99.8% | 24 |

`linux-x86_64-v3`:

| Scenario | fRust CPU as % of official | Pairs |
| --- | --- | --- |
| cold build | 100.2% | 32 |
| clean rebuild | 45.3% | 32 |
| no-op rebuild | 88.8% | 32 |
| edit rebuild, leaf | 99.8% | 24 |
| edit rebuild, wide | 99.5% | 24 |

`macos-arm64`:

| Scenario | fRust CPU as % of official | Pairs |
| --- | --- | --- |
| cold build | 100.1% | 32 |
| clean rebuild | 62.3% | 32 |
| no-op rebuild | 105.5% | 32 |
| edit rebuild, leaf | 104.6% | 24 |
| edit rebuild, wide | 104.1% | 24 |

`macos-x86_64`:

| Scenario | fRust CPU as % of official | Pairs |
| --- | --- | --- |
| cold build | 101.5% | 32 |
| clean rebuild | 59.3% | 32 |
| no-op rebuild | 103.3% | 32 |
| edit rebuild, leaf | 100.0% | 24 |
| edit rebuild, wide | 98.9% | 24 |

### Linux, per workload

fRust CPU as a percent of official, 8 pairs in every cell. The edit rebuilds are only in the all-workloads tables above.

| Workload | Cold, x86-64 | Cold, v3 | Clean, x86-64 | Clean, v3 | No-op, x86-64 | No-op, v3 |
| --- | --- | --- | --- | --- | --- | --- |
| ripgrep | 99.8% | 100.1% | 58.4% | 58.2% | 85.2% | 87.3% |
| syn | 100.6% | 99.7% | 75.4% | 75.1% | 94.0% | 92.8% |
| regex | 100.4% | 101.1% | 68.7% | 68.6% | 87.7% | 86.2% |
| fd | 101.0% | 100.1% | 14.2% | 14.1% | 90.7% | 89.2% |

### Reading the results

What wins:

- Clean rebuild. fRust used 45.5% and 45.3% of official CPU on Linux and 62.3% and 59.3% on macOS. On Linux it was faster in every one of the 8 pairs of every workload. On macOS it was faster in every pair but one (a regex pair on `macos-x86_64`), although the per-pair ranges there are wide. The effect differs by workload: on `linux-x86_64`, fd is at 14.2% and syn at 75.4%.
- No-op rebuild on Linux. 89.3% and 88.8%, faster in all four workloads. In 7 of 8 pairs for fd and in all 8 for the others. These builds are short: the official CPU is 15.2 to 44.1 ms on the baseline runners and 17.0 to 37.1 ms on the v3 runners.

What ties:

- Cold builds on Linux. 100.5% and 100.2%. fRust does not beat official on cold builds: it was faster in only 1 of 4 workloads on each Linux platform.
- Edit rebuilds on Linux. Leaf 100.1% and 99.8%, wide 99.8% and 99.5%. Everything is within about half a percent of official, either way. The v3 build is slightly ahead of official on edit rebuilds, faster in all three workloads for both edits, by less than one percentage point.

What this run cannot resolve:

- macOS cold, no-op and edit rebuilds. The macOS runners are virtual machines on shared hosts and nothing is pinned. The per-pair ranges are wide, for example ripgrep cold 70.8% to 124.3% and regex no-op 79.1% to 176.8% on `macos-arm64`, and regex cold 79.1% to 147.0% on `macos-x86_64`. A difference of a few percent cannot be told from noise, so none of these numbers is reported as a win or a loss.

Why the numbers look like this:

- The clean rebuild result comes from the unit cache. The fRust clean rebuild restores dependency outputs from the store that the same round's cold build filled (for example 59 hits and 0 misses for fd on Linux), and official Rust has no store, so it compiles everything again.
- Cold builds start with an empty store. The median number of store hits per fRust cold build is 0 for every workload, so there is nothing to restore, and the build also pays for writing the entries.
- The Linux x86-64-v3 build has its compiler and tools built for x86-64-v3, so it requires a CPU with AVX2, BMI1, BMI2, FMA, MOVBE and the other extensions listed in [Installer variants](configuration.md#installer-variants). `frust-install` picks it automatically on a supporting CPU and falls back to the baseline build otherwise.
- The toolchains were compared in the page-cache state that their installers leave, which is the state a user gets. The page-cache table of the report (MiB of `librustc_driver` and `libLLVM` text mapped with 2 MiB pages, before and after the builds) reads 48 / 48 for the official toolchain in every Linux job. For fRust it reads 48 / 48 on `linux-x86_64` and 52 / 48 on `linux-x86_64-v3`. Both installers therefore leave large pages, and neither toolchain ran with 4 KiB pages (0 in that table). The probe runs on Linux only, so macOS has no such record.

## Rules for new measurements

Compare fRust with the same upstream Rust and Cargo revision, under the same optimization, debug, panic, target, codegen-unit and linker settings. Include an upstream release baseline with incremental compilation and full LTO explicitly enabled whenever fRust uses those settings. Record the actual compiler and linker invocations.

Report clean builds with cold artifacts, Cargo no-op builds, local implementation edits, wider dependency changes and native relinks separately. For source edits, verify the edited target ran and log which phases did fresh work. Verify the edited result against a fresh build before counting reuse as a win. Record elapsed time, tail latency, peak memory and disk use on the stated host.

Measure runtime and executable size independently. Compare equivalent stripped artifacts and record debug-symbol size separately. Publish the command, inputs, toolchain versions, host details, seed state and repeat measurements for any public performance claim.

The frozen leaf edits sit inside the inlining cone of their callers, and the ripgrep leaf edit leaves the optimized IR unchanged. `qualification/PROTOCOL.md` says they cannot establish a speedup on meaningful binary fat-LTO edits. `qualification/PROTOCOL.md` says a successor cohort will be frozen before new candidate timing.

Timings from earlier versions of fRust, the retired build accelerator, describe a different architecture and are not comparable.
