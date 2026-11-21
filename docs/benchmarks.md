# Benchmark methodology and results

No fRust compiler change has passed the frozen Linux budgets. Nothing here is a qualified faster-rebuild result. The 20 November close-out, with each gate marked passed or failed, is in `qualification/GATE-REPORT.md`. Earlier candidate logs and hashes are in `qualification/CANDIDATE.md`. The raw logs live in a local, git-ignored build directory and are not in the repository, so a clone cannot recheck these numbers.

## Conditions

- Host: Linux x86-64. The CPU model, kernel and linker versions are not recorded in the qualification files. macOS was not measured.
- Dates: the candidate samples in the table below were taken on 2026-11-19, after the protocol was frozen that day (`qualification/PROTOCOL.md`). The 20 November full paired judges are the interface-hash cutoff and the prelink-span compiler, recorded after that table. The same day's x86-64-v3 preflight is a three-workload runtime and size check, not a paired build-time judge. The same day's outliner screen is one ripgrep stripped-size build, not a paired judge. The same day's loop-fusion check is a bitcode comparison, not a paired judge. The same day's Attributor screen is a ripgrep release build that crashed in LLVM, not a paired judge. The same day's NewGVN check is a bitcode comparison, not a paired judge. The same day's GVN-sink check is a bitcode comparison, not a paired judge. The 21 November fat-LTO simplification-gap check is a bitcode comparison, not a paired judge. The same day's joint-candidate check did not build a new compiler. The same day's cache-and-size preflight measured the span caches and `.rodata` duplicates and did not build a compiler. The same day's full-LTO preflight without RELR did not build a compiler. The same day's four-budget scan of the saved judges did not package a compiler. The same day's MIR-inlining preflight built ripgrep with the pass forced on and did not package a compiler. The same day's dataflow const-prop preflight forced that MIR pass on one ripgrep build and did not package a compiler. The same day's MIR opt level 3 measurement ran the inliner and limited const-prop together, collected leaf, disk, runtime, and stripped-size samples, and did not package a compiler. The same day's follow-up did not start the 630-pair cohort: the runtime geomean is already 0.99079798, and moving the span caches out of the trace does not change those binaries. The same day's joint preflight was not started: hotbudget, the fastest judged stack, has runtime geomean 0.97299293, and its ripgrep file grew. The same day's six-switch backend preflight was measured: leaf geomean 0.99252570, disk 1.00071044, runtime upper 0.99910136, and rustfmt grew from 3835192 to 3835544.
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
