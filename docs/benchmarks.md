# Benchmark methodology and results

No fRust compiler change has passed the frozen Linux budgets. Nothing here is a qualified faster-rebuild result. The detail, including logs and hashes, is in `qualification/CANDIDATE.md`. The raw logs live in a local, git-ignored build directory and are not in the repository, so a clone cannot recheck these numbers.

## Conditions

- Host: Linux x86-64. The CPU model, kernel and linker versions are not recorded in the qualification files. macOS was not measured.
- Dates: every candidate sample below was taken on 2026-11-19, after the protocol was frozen that day (`qualification/PROTOCOL.md`).
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
| Post-LTO object reuse plus RELR (`frust-0ef1bc386-cgcache1`, `frust-cg1`) | Single samples, see below. | Kept, not qualified. Gate not run. |

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

These results come from the Benchmark workflow (`clean` and `noop` series, control against fRust, both built by the Toolchains workflow). No workflow run is recorded here yet; each recorded result will name its run and commit and contain no local-host numbers. Local-host development measurements, taken on one machine with pinned cores, are in `qualification/OPTIMIZED-LINUX.md` and are not release claims. A first build with an empty store also pays for writing cache entries.

## Rules for new measurements

Compare fRust with the same upstream Rust and Cargo revision, under the same optimization, debug, panic, target, codegen-unit and linker settings. Include an upstream release baseline with incremental compilation and full LTO explicitly enabled whenever fRust uses those settings. Record the actual compiler and linker invocations.

Report clean builds with cold artifacts, Cargo no-op builds, local implementation edits, wider dependency changes and native relinks separately. For source edits, verify the edited target ran and log which phases did fresh work. Verify the edited result against a fresh build before counting reuse as a win. Record elapsed time, tail latency, peak memory and disk use on the stated host.

Measure runtime and executable size independently. Compare equivalent stripped artifacts and record debug-symbol size separately. Publish the command, inputs, toolchain versions, host details, seed state and repeat measurements for any public performance claim.

The frozen leaf edits sit inside the inlining cone of their callers, and the ripgrep leaf edit leaves the optimized IR unchanged. `qualification/PROTOCOL.md` says they cannot establish a speedup on meaningful binary fat-LTO edits. `qualification/PROTOCOL.md` says a successor cohort will be frozen before new candidate timing.

Timings from earlier versions of fRust, the retired build accelerator, describe a different architecture and are not comparable.
