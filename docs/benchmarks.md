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

## Real repositories and CI

`frust bench --repo PATH` builds an existing checkout with plain Cargo (isolated `CARGO_TARGET_DIR`) and with frust (isolated `FRUST_HOME` and central target), using deferred stores and automatic compression. Dependencies are fetched once before timing.

```bash
frust bench --repo ~/src/ripgrep --runs 3 --edits 20 --gate --markdown --out ripgrep.json
frust bench --repo ~/src/tokio --package tokio --json
```

Flags: `--repo PATH`, `--edits N` (default 20), `--runs N` (default 3), `--package P` (a workspace member name or `name@version`, resolved through `cargo metadata` to its path package id and passed as `cargo build -p`; edits come from that member only; without it the build is `--workspace`), `--features LIST` (comma-separated, passed as `--features` to both tools), `--disk-slack PCT` (default 0: the disk guard fails when frust's total exceeds Cargo's by more than PCT percent), `--json`, `--markdown`, `--out FILE`, `--gate`.

Each run times, for both tools in alternating order: cold build (empty target and cache), clean rebuild (cache kept, target removed), no-op (median of three builds), then N edits. Edits append a marker constant to up to eight `.rs` files, taken round-robin from the entry points and sources of all workspace path members (`cargo metadata --no-deps`), each followed by a debug build. The first edit that touches a crate after a restore has no incremental session; it is reported separately (`edit_first`) and excluded from the edit median and p90 (`edit_warm`). Original file bytes are saved in scratch and written back after every run and on every error path; git is never used.

Before each timed frust build the harness waits up to 60 s for deferred stores to finish (no pending job files, every slot lock held exclusively), so background storage never lands in the next row.

Disk compares the physical size of Cargo's target directory with the physical size of all of `FRUST_HOME` (store, targets, manifests, tmp), counting each inode and clone extent once. The exit code is 1 when frust is larger, a build fails, or the report cannot be written. With `--gate` the exit code is 2 when a time threshold fails, using the median of per-pair frust/Cargo ratios, each with an absolute allowance so builds of a few hundred milliseconds are not judged on ratio alone:

| Row | Limit |
| --- | --- |
| No-op | at most 1.00x + 15 ms (25 ms on Windows) and at most 1.25x |
| Warm-session edit median | at most 1.05x + 25 ms |
| Edit p90 | at most 1.15x + 40 ms |
| Cold | at most 1.05x + 150 ms |
| Clean rebuild from cache | at most 0.5x + 100 ms |

The no-op row is a fixed-cost check, so it keeps the tight 1.25x ratio; the wider Windows allowance applies to the additive limit only.

The report also carries a `disk_breakdown` object (additive; schema version 1): physical and logical bytes of Cargo's target and of frust's `store`, `targets`, `manifests`, `tmp` and everything else, each inode and clone extent counted once; the targets-only comparison; the number of cache entries; and the share of store bytes whose extents are also referenced from targets. `other_trees` splits `other` by top-level name under `FRUST_HOME`, largest first, with files, directories and physical bytes per tree (the markdown prints it as an "Other, by tree" line), so metadata overhead is attributed to a directory such as `locks`, `probe` or `history`. `frust_physical_bytes_before_maintenance` repeats the pre-maintenance total next to the post-maintenance `frust_physical_bytes`.

The disk figure is taken after one explicit maintenance pass on the frust home: the daily gc and a full compaction of the central target (the same calls the product runs automatically after builds, which the timed builds disable with `FRUST_AUTO_COMPACT=off` to stay deterministic), then one gc that also unlinks every idle compile lock file. Cargo has no equivalent, so its target is measured as is. `disk.frust_bytes` is the post-maintenance total and is what the disk guard gates on; `disk.frust_bytes_before_maintenance` and `disk.maintenance_ms` are reported beside it, so both figures stay visible.

Two more additive sections explain where bytes and time go. `store_only` reads every `store/<key>/meta.json` (or the legacy `store/<shard>/<key>/meta.json`) and, for each payload file, measures the bytes no file under the central targets shares by hardlink or cloned extent, using the same allocation helper as the disk totals, probing 63 files per allocation query. It reports the total, the 20 largest entries (crate, version, `source`, file kinds, file count, bytes) and totals by crate class (`registry-git`, `path`, `unknown` from the `source` field, plus `build-script` for `build_script_*` crates and `proc-macro` for entries holding a host dylib or named `*_derive`, `*_macro` or `*_macros`) and by file kind (`rlib`, `rmeta`, `dylib` for so, dylib and dll, `bin` for exe and extensionless outputs, `d`, `other`). When the allocation helper fails the section reports `status: unsupported` with the reason instead of an estimate. `frust_timings` sets `FRUST_TIMINGS` on every frust build (format in `how-it-works.md`, Timing records) and sums the wrapper records per row (`cold`, `clean_rebuild`, `noop`, `edit_first`, `edit_warm`): builds, units, `total_ms`, `rustc_ms`, overhead (`total_ms - rustc_ms`), each stage and the count per `store_mode`. The timing files are deleted after each build. Records are read with defaults on every field, so the optional `extern_files`, `extern_bytes_hashed`, `extern_bytes_memo` and `native_dir_bytes_hashed` fields are summed per row when the wrapper writes them and count as zero otherwise. `drain_ms` per row is the time from each frust build's exit until its deferred stores finished: the harness waits for that drain, including queued store jobs and a helper it starts itself when nobody is draining, before the next row begins, so store cost off the timed path stays visible. For every row, `miss_units` and `miss_crates` count the records whose `store_mode` is not `hit`, and `misses` lists the 20 slowest of them (crate, `path_unit`, `primary`, `incremental`, `rustc_ms`, `store_mode`); the markdown shows the top 10 for the clean rebuild and no-op rows as "units that compiled in this row". The markdown also prints one line with the cold row's store modes (inline, queued, deferred, hit) for the OS of the job. Gate checks carry `ratio_min` and `ratio_max`, the smallest and largest per-pair ratio next to the median the gate uses. The workflow runs five paired runs per job.

`--markdown` prints a table (cold, clean rebuild, no-op, first edit, warm edit, edit p90, disk, plus a frust/cargo ratio row and, with `--gate`, a gate column) the disk breakdown, the store-only summary with its top 10 entries, and a "where the time goes" table for `$GITHUB_STEP_SUMMARY`; `--json` prints a versioned report; `--out FILE` always writes the JSON to the file while stdout carries the markdown (or the JSON when only `--json` is given). The thresholds are constants in `src/bench/repo.rs`.

`.github/workflows/bench.yml` runs this gate on ubuntu, macOS and Windows free runners against ripgrep 14.1.1, bat v0.24.0 and tokio 1.40.0 (package `tokio@1.40.0` with `--features full`, since tokio has no default features), on manual dispatch, weekly, and on pull requests touching `src/**`, `Cargo.toml` or the workflow. Each job uploads its JSON as an artifact and removes the clone and scratch state at the end.
