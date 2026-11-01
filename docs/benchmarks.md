# Benchmarks

## Method

Tables show milliseconds, median [minimum, maximum] across seven repetitions. Artifact-cold starts use empty targets/caches and warm OS pages. The general graph has four local libraries plus registry dependencies; the embedded matrix has two. First edits after restores and warm-session edits are separate. Outputs are verified; no RAM comparison is measured.

General, profile and concurrent timings use deferred stores and automatic compression. Foreground includes frontend, Cargo and audit. Settled adds drain: no pending jobs while all existing slot locks are held together. Deferred stores drain between commands. No-ops require zero compiler calls, compilation notices and cache events.

Matching canonical flags are LTO=false/16 codegen units/unwind, thin/16/unwind and fat/1/abort, all incremental=false. LTO=false permits thin local LTO. Release figures use this matrix. Same-path and restored binaries match; measured executables run.

Growth uses synchronous stores: 200 mixed body/signature/feature/revert edits across 12 libraries and the application. Source settling and allocation inspection are outside build clocks. Compaction **every build** is controlled maintenance. Concurrent default-dev groups use separate targets, independently warmed compiler-info, an empty shared cache and alternating tool/spawn order; peer reuse is expected.

Use matching release-built siblings and an existing TMPDIR on the filesystem being measured:

```bash
frust bench --quick --runs=7 --store=deferred --compression=auto --json
frust bench --growth --settle-edits --edits=200 --every=20 --libs=12 --variants=cargo,auto --compression=auto --json --keep-scratch
frust bench --linking --quick --runs=7 --json
frust bench --concurrent --quick --runs=7 --store=deferred --compression=auto --json
```

## Results

Measured 2026-11-01 on Core Ultra 7 255H, 16 logical CPUs, Linux 7.0.0-42-generic x86_64, ext4, rustc 1.97.1 and Cargo 1.97.1. Reflinks were unavailable. One-minute load before/after was general 1.19/2.32, embedded profiles 2.36/2.36, growth 2.32/2.83, linking 2.83/2.76, concurrent 2.76/3.32.

**Debug builds**

| Operation | Cargo ms | frust ms |
| --- | --- | --- |
| Cold | 3395 [2840, 3584] | 3577 [3003, 3679] |
| Second project | 3477 [2854, 3565] | 827 [710, 865] |
| Clean restore | 3403 [2812, 3621] | 162 [157, 168] |
| No-op | 18 [17, 19] | 19 [18, 22] |
| First edit after restore | 93 [85, 115] | 99 [94, 107] |
| Warm-session edit | 94 [85, 123] | 103 [97, 123] |

Warm-session edits: paired frust-minus-Cargo median 13 [-20, 25] ms; frust slower in 5/7 pairs, faster in 2/7, tied in 0/7.

**Canonical release profiles**

| Profile | Operation | Cargo ms | frust ms |
| --- | --- | --- | --- |
| Defaults | Cold | 3623 [3539, 3772] | 3790 [3728, 3853] |
| Defaults | Restore | 3717 [3674, 3814] | 142 [140, 146] |
| Defaults | No-op | 18 [17, 20] | 20 [19, 20] |
| Defaults | Body edit | 69 [66, 72] | 74 [73, 76] |
| ThinLTO | Cold | 5081 [5027, 5166] | 5190 [5134, 5278] |
| ThinLTO | Restore | 5070 [4940, 5235] | 150 [141, 168] |
| ThinLTO | No-op | 18 [17, 19] | 19 [18, 20] |
| ThinLTO | Body edit | 1794 [1761, 1825] | 1807 [1725, 1882] |
| Full/fat LTO | Cold | 4990 [4941, 5026] | 5089 [5014, 5183] |
| Full/fat LTO | Restore | 4994 [4875, 5127] | 144 [139, 149] |
| Full/fat LTO | No-op | 19 [17, 19] | 20 [19, 21] |
| Full/fat LTO | Body edit | 1664 [1652, 1755] | 1696 [1661, 1789] |

Frust had positive paired cold overhead in all four graphs. Differences pair matching repetitions, frust minus Cargo:

| Cold graph | Paired difference ms | Positive pairs |
| --- | --- | --- |
| Debug | 163 [-142, 287] | 5/7 |
| Defaults | 150 [61, 286] | 7/7 |
| ThinLTO | 112 [15, 203] | 7/7 |
| Full/fat LTO | 128 [9, 198] | 7/7 |

**Deferred store completion**

Foreground figures above exclude the remaining background store work. These are frust observations from the same seven samples.

| Operation | Store drain ms | Settled total ms |
| --- | --- | --- |
| Cold | 10 [10, 30] | 3587 [3013, 3709] |
| Clean restore | 0 [0, 0] | 162 [157, 169] |
| First edit after restore | 20 [0, 20] | 118 [95, 127] |
| Warm-session edit | 10 [0, 20] | 116 [103, 134] |

**Concurrent cold groups**

| Measurement | Cargo ms | frust ms | Paired difference ms |
| --- | --- | --- | --- |
| Foreground group | 5091.110 [5041.489, 5202.440] | 3715.133 [3518.938, 3854.137] | -1439.871 [-1640.067, -1260.774] |
| Settled group | 5091.150 [5041.523, 5202.476] | 3715.222 [3519.062, 3854.262] | -1439.793 [-1640.014, -1260.718] |

Frust foreground was faster in 7/7 pairs; settled was faster in 7/7. Actual Cargo-unit compilations per cold group were **40 Cargo / 20 frust**, with **14 shared identities** compiled once and corroborated by store/peer hits. Binary bytes/runtime output matched across tools/profiles. Unchanged groups had zero compiler calls, compilation notices and cache events. These measure concurrent peer reuse.

**Storage after 200 settled edits**

| Tool | Target MiB | Additional cache MiB | Other managed MiB | Full managed MiB |
| --- | --- | --- | --- | --- |
| Cargo | 371.2 | 0.0 | 0.0 | 371.2 |
| frust + Auto | 174.2 | 48.6 | 3.1 | 225.8 |

Full managed allocation was **39.2% smaller** with frust. Unique device/inode blocks include directories, excluding installed toolchains/registries. MiB components round independently; retired allocation is not added twice.

Each variant recorded 202 matching outputs, 201 positive-slack settlements and zero final unchanged-build compiler calls. Cumulative non-query compiler invocations, including capability probes, were **1,542 Cargo / 1,542 frust**. Frust retained at most **2 keys/unit**, within the bound of **4**, with **0 missing entries** and **0 retired payload entries (0 bytes)** included in full managed allocation. These single sequences check backing presence, not complete restore integrity.

**Native linking baseline**

With FRUST_LINK=off and artifact caching off, every edit compiled one main binary with expected output. Benchmark-only RUSTC_BOOTSTRAP=1 and -Ztime-passes profile application link_binary, excluding dependencies. The configured linker is preserved; only its baseline is measured.

| Measurement | ms median [min, max] |
| --- | --- |
| Total build wall | 87 [84, 90] |
| Native link phase | 32 [30, 34] |

Cohort: `07c77c1abcb91564c1bdb0948c7961c780b74fd5f00aef2695cc68ed21d90e70`.
