# Benchmarks

## Method

Tables show milliseconds, median [minimum, maximum] across seven repetitions. Artifact-cold starts use empty targets/caches and warm OS pages. The general graph has four local libraries plus registry dependencies; the embedded matrix has two. First edits after restores and warm-session edits are separate. Outputs are verified; no RAM comparison is measured.

General, profile and concurrent timings use deferred stores and automatic compression. Foreground includes frontend, Cargo and audit. Settled adds drain: no pending jobs while all existing slot locks are held together. Deferred stores drain between commands. No-ops require zero compiler calls, compilation notices and cache events.

Matching canonical flags are LTO=false/16 codegen units/unwind, thin/16/unwind and fat/1/abort, all incremental=false. LTO=false permits thin local LTO. Release figures use this matrix. Same-path and restored binaries match; measured executables run.

Growth uses synchronous stores: 200 mixed body/signature/feature/revert edits across 12 libraries and the application. Source settling and allocation inspection are outside build clocks. Compaction **every build** is controlled maintenance. Concurrent default-dev groups use separate targets, independently warmed compiler-info, an empty shared cache and alternating tool/spawn order; peer reuse is expected.

A separate five-pair comparison against official 0.2.0 uses an 11-package graph and two concurrent release builds (LTO=false, CGU=16, incremental=false, one Cargo job each). Both versions use the same paths and backend wrapper; order alternates and native compilation overlap is recorded.

Use matching release-built siblings and an existing TMPDIR on the filesystem being measured:

```bash
frust bench --quick --runs=7 --store=deferred --compression=auto --json
frust bench --growth --settle-edits --edits=200 --every=20 --libs=12 --variants=cargo,auto --compression=auto --json --keep-scratch
frust bench --linking --quick --runs=7 --json
frust bench --concurrent --quick --runs=7 --store=deferred --compression=auto --json
```

## Results

Measured 2026-11-01 on Core Ultra 7 255H, 16 logical CPUs, Linux 7.0.0-42-generic x86_64, ext4, rustc 1.97.1 and Cargo 1.97.1. Reflinks were unavailable. One-minute load before/after was general 0.98/2.04, embedded profiles 2.27/2.54, growth 2.04/2.43, linking 2.47/2.47, concurrent 2.44/3.98.

**Debug builds**

| Operation | Cargo ms | frust ms |
| --- | --- | --- |
| Cold | 3127 [2812, 3627] | 3316 [2892, 3471] |
| Second project | 3279 [2786, 3483] | 814 [685, 833] |
| Clean restore | 3311 [2776, 3443] | 157 [152, 162] |
| No-op | 18 [17, 19] | 19 [18, 21] |
| First edit after restore | 88 [86, 97] | 107 [93, 119] |
| Warm-session edit | 89 [85, 92] | 102 [95, 109] |

Warm-session edits: paired frust-minus-Cargo median 15 [3, 18] ms; frust slower in 7/7 pairs, faster in 0/7, tied in 0/7.

**Canonical release profiles**

| Profile | Operation | Cargo ms | frust ms |
| --- | --- | --- | --- |
| Defaults | Cold | 3701 [3491, 3870] | 3830 [3772, 4527] |
| Defaults | Restore | 3691 [3531, 3902] | 146 [139, 159] |
| Defaults | No-op | 18 [17, 19] | 20 [18, 24] |
| Defaults | Body edit | 70 [68, 77] | 73 [71, 80] |
| ThinLTO | Cold | 5040 [5000, 5191] | 5275 [5126, 5344] |
| ThinLTO | Restore | 5138 [5068, 5180] | 143 [140, 149] |
| ThinLTO | No-op | 18 [17, 19] | 20 [18, 20] |
| ThinLTO | Body edit | 1751 [1715, 1932] | 1797 [1768, 1898] |
| Full/fat LTO | Cold | 4996 [4959, 5068] | 5146 [5044, 5211] |
| Full/fat LTO | Restore | 5060 [4872, 5122] | 144 [140, 155] |
| Full/fat LTO | No-op | 18 [17, 19] | 20 [18, 22] |
| Full/fat LTO | Body edit | 1688 [1663, 1734] | 1702 [1668, 1756] |

Frust had positive paired cold overhead in all four graphs. Differences pair matching repetitions, frust minus Cargo:

| Cold graph | Paired difference ms | Positive pairs |
| --- | --- | --- |
| Debug | 80 [-378, 377] | 5/7 |
| Defaults | 149 [-31, 782] | 6/7 |
| ThinLTO | 164 [39, 336] | 7/7 |
| Full/fat LTO | 126 [78, 221] | 7/7 |

**Deferred store completion**

Frust observations from the same seven samples; foreground excludes remaining background work.

| Operation | Store drain ms | Settled total ms |
| --- | --- | --- |
| Cold | 10 [10, 20] | 3326 [2902, 3491] |
| Clean restore | 0 [0, 0] | 158 [152, 162] |
| First edit after restore | 10 [0, 20] | 117 [93, 130] |
| Warm-session edit | 10 [0, 20] | 113 [95, 129] |

**Concurrent cold groups**

| Measurement | Cargo ms | frust ms | Paired difference ms |
| --- | --- | --- | --- |
| Foreground group | 5051.008 [4730.561, 5229.465] | 3703.126 [3469.977, 3830.437] | -1341.844 [-1562.016, -1085.113] |
| Settled group | 5051.049 [4730.599, 5229.653] | 3703.262 [3470.079, 3830.573] | -1341.768 [-1562.074, -1085.140] |

Frust foreground was faster in 7/7 pairs; settled was faster in 7/7. Actual Cargo-unit compilations per cold group were **40 Cargo / 20 frust**, with **14 shared identities** compiled once and corroborated by store/peer hits. Binary bytes and runtime output matched between Cargo and frust. Unchanged groups had zero compiler calls, compilation notices and cache events. These measure concurrent peer reuse.

**Release cold build cooperation**

| Measurement | 0.2.0 ms | 0.3.0 ms | Paired 0.3.0-minus-0.2.0 ms |
| --- | --- | --- | --- |
| Foreground | 5427 [5385, 5614] | 4108 [4007, 4342] | -1314 [-1419, -1198] |
| Settled | 5431 [5388, 5628] | 4110 [4010, 4346] | -1317 [-1420, -1207] |

All five 0.3.0 pairs finished sooner; the paired median foreground reduction was **24.2%**. Independent compilation overlapped in all five groups. Owned CPU increased by a paired median **703 ms (12.9%)**. These results cover this concurrent fixture.

**Storage after 200 settled edits**

| Tool | Target MiB | Additional cache MiB | Other managed MiB | Full managed MiB |
| --- | --- | --- | --- | --- |
| Cargo | 371.2 | 0.0 | 0.0 | 371.2 |
| frust + Auto | 174.2 | 48.6 | 3.0 | 225.8 |

Full managed allocation was **39.2% smaller** with frust. Unique device/inode blocks include directories, excluding installed toolchains/registries. MiB components round independently; retired allocation is not added twice.

Each variant recorded 202 matching outputs, 201 positive-slack settlements and zero final unchanged-build compiler calls. Cumulative non-query compiler invocations, including capability probes, were **1,542 Cargo / 1,542 frust**. Frust retained at most **2 keys/unit**, within the bound of **4**, with **0 missing entries** and **0 retired payload entries (0 bytes)** included in full managed allocation. These single sequences check backing presence, not complete restore integrity.

**Native linking baseline**

With FRUST_LINK=off and artifact caching off, every edit compiled one main binary with expected output. Benchmark-only RUSTC_BOOTSTRAP=1 and -Ztime-passes profile application link_binary, excluding dependencies. The configured linker is preserved; only its baseline is measured.

| Measurement | ms median [min, max] |
| --- | --- |
| Total build wall | 86 [84, 100] |
| Native link phase | 32 [30, 48] |

Cohort: `8199243f9160c65744f7285ae3fac0e0793beff0aee416cf7d2bab5f90147fe1`.
