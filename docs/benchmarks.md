# Benchmark methodology and results

No fRust compiler change has passed the frozen Linux budgets. Nothing here is a qualified faster-rebuild result. The 20 November close-out, with each gate marked passed or failed, is in `qualification/GATE-REPORT.md`. Earlier candidate logs and hashes are in `qualification/CANDIDATE.md`. The raw logs live in a local, git-ignored build directory and are not in the repository, so a clone cannot recheck these numbers.

## Conditions

- Host: Linux x86-64. The CPU model, kernel and linker versions are not recorded in the qualification files. macOS was not measured.
- Dates: the candidate samples in the table below were taken on 2026-11-19, after the protocol was frozen that day (`qualification/PROTOCOL.md`). The 20 November full paired judges are the interface-hash cutoff and the prelink-span compiler, recorded after that table. The same day's x86-64-v3 preflight is a three-workload runtime and size check, not a paired build-time judge. The same day's outliner screen is one ripgrep stripped-size build, not a paired judge. The same day's loop-fusion check is a bitcode comparison, not a paired judge. The same day's Attributor screen is a ripgrep release build that crashed in LLVM, not a paired judge.
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

These results come from the Benchmark workflow (the official comparison described in [CI measurement method](#ci-measurement-method)). No workflow run is recorded here yet; each recorded result will name its run and commit and contain no local-host numbers. Local-host development measurements, taken on one machine with pinned cores, are in `qualification/OPTIMIZED-LINUX.md` and are not release claims. A first build with an empty store also pays for writing cache entries.

## CI measurement method

The Benchmark workflow (`.github/workflows/benchmark.yml`) is started by hand and holds two measurements. Neither writes a number into this repository. Numbers are published only from a completed run, by hand, with the run and commit named.

### Official comparison

Official Rust against the fRust toolchain with its Cargo unit cache, on three GitHub-hosted platforms:

| Platform | Runner label | Target |
| --- | --- | --- |
| `linux-x86_64` | `ubuntu-24.04` | `x86_64-unknown-linux-gnu` |
| `macos-arm64` | `macos-latest` | `aarch64-apple-darwin` |
| `macos-x86_64` | `macos-15-intel` | `x86_64-apple-darwin` |

- Official toolchain: `rustup toolchain install 1.100.0 --profile minimal`, pinned to that exact version. The driver refuses to run unless `rustc -Vv` reports `release: 1.100.0`.
- fRust toolchain: the packaged stage-2 toolchain built natively on a runner of the same kind by the Toolchains workflow, run as shipped through `fcargo` and `frustc`. The experimental switches are off unless the `compare-experimental` input turns them on. The published binaries of official Rust are PGO builds and the fRust recipe is not, which is why the frozen stock budgets use the unpatched stage-2 of the same recipe instead (`qualification/PROTOCOL.md`).
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
- A job starts no round that would end after 300 minutes, so jobs stay inside the six hour limit of a hosted runner. A run cut short this way records a stop line and the report states how many pairs it has.

### Recorded environment

Each raw result file starts with a meta record: platform label, runner CPU model, physical and logical core counts (and performance and efficiency cores on Apple silicon), OS, memory, pinning mode, `-j`, both toolchains' `cargo -V` and `rustc -Vv` output, the repository commit, the workflow run, the cohort commit and `Cargo.lock` hash, the profile and the fRust environment. The report lists every distinct host, because runner hardware varies between jobs of one platform.

### Output

Every `compare` job uploads its raw JSON lines and cargo logs as `compare-<platform>-<workload>`. The `report` job downloads them all and uploads `benchmark-report`, with `results.md`, `summary.json` and one `chart-<platform>.svg` per platform. Each chart has one horizontal bar per scenario and workload, with 100 percent marked. The job also writes the table to the run summary and commits nothing.

### Limits

- Shared runners are noisy and their hardware is not chosen. The ratios are within-job, so they do not depend on the runner, but they are valid only for the hardware recorded with them.
- The edit scenarios use the frozen edits, which sit inside the inlining cone of their callers (see below). They are not a general edit workload.
- The official comparison does not use `qualification/time-build.sh`, whose empty-target check uses `find -quit`, which is GNU specific, and which always runs plain `cargo` rather than `fcargo`. The protocol jobs still use it, turn the unit cache on with `FRUST_UNIT_CACHE=1`, and stay on Linux x86-64.
- The runtime and stripped size gates are not part of the official comparison.

## Rules for new measurements

Compare fRust with the same upstream Rust and Cargo revision, under the same optimization, debug, panic, target, codegen-unit and linker settings. Include an upstream release baseline with incremental compilation and full LTO explicitly enabled whenever fRust uses those settings. Record the actual compiler and linker invocations.

Report clean builds with cold artifacts, Cargo no-op builds, local implementation edits, wider dependency changes and native relinks separately. For source edits, verify the edited target ran and log which phases did fresh work. Verify the edited result against a fresh build before counting reuse as a win. Record elapsed time, tail latency, peak memory and disk use on the stated host.

Measure runtime and executable size independently. Compare equivalent stripped artifacts and record debug-symbol size separately. Publish the command, inputs, toolchain versions, host details, seed state and repeat measurements for any public performance claim.

The frozen leaf edits sit inside the inlining cone of their callers, and the ripgrep leaf edit leaves the optimized IR unchanged. `qualification/PROTOCOL.md` says they cannot establish a speedup on meaningful binary fat-LTO edits. `qualification/PROTOCOL.md` says a successor cohort will be frozen before new candidate timing.

Timings from earlier versions of fRust, the retired build accelerator, describe a different architecture and are not comparable.
