# Measured results

## Methodology

`frust bench` times plain cargo and frust against each other on two generated projects, in an isolated state
directory under the system temp directory that it removes afterwards.

* **Runs.** `--runs N` sets how many times every timed row is measured: 5 by default, 3 with `--quick`, and never
  fewer than 3 (a smaller value is an error). `--quick` also uses 4 local crates instead of 12.
* **Median and spread.** Each row reports the median of its runs, with the minimum, maximum and run count beside it.
  Speed-up ratios are ratios of medians. The release binary's run time is a separate median over 20 launches (10
  with `--quick`).
* **Warm-up.** Before timing, an untimed build of a trivial project runs once per tool and profile (debug and
  release), so first-launch costs of rustc, std, the linker and `fcargo` are not charged to whichever row runs first.
* **Order.** Within every run, which tool goes first alternates, so neither one always pays for the other's cold
  page cache.
* **Maintenance kept out of the speed rows.** frust builds in the timed rows run with automatic compaction off and
  the daily gc marker made fresh, so inline maintenance is never absorbed into a build time. The two passes are timed on
  their own, once, on the populated state, as rows named `maintenance: daily gc pass (once)` and
  `maintenance: auto-compaction (once)`; the compaction row lifts the pass's time and byte budget.
* **Display.** The display pipeline is off in every other frust row. One extra row, `clean rebuild, display on`,
  repeats the clean rebuild with the display enabled (plain rendering) to show what it costs.
* **Rows.** For debug: project A cold, project B cold (cargo) and cold cache and warm cache (frust), clean rebuild,
  clean rebuild with display on (frust only), no-op build, the first edit after the clean rebuild, and a second
  edit with warm incremental sessions. The original `incremental edit` row retains the recovery cost after a cache
  restore: frust has restored artifacts but has no incremental session yet, whereas cargo's clean compile created
  one. The `incremental edit, warm session` row measures ongoing edits after both tools have actually compiled. For release: A and B (cargo
  cold; frust cold cache for A and warm cache for B) and a clean rebuild. Then the two maintenance rows. Disk use of the two projects, a configuration-churn
  test and the release-binary comparisons are reported after the timed rows.
* **Machine context.** The report states the operating system, CPU, core count and the load average before and
  after the run (where the platform has one), so a number taken on a busy machine can be recognised as such.
* **Exit code.** `frust bench` exits 0 on success and 1 on an error or when a correctness check fails: the release
  binary restored from the cache must be byte-identical to the cold build and to plain cargo's in the same target
  directory, and the two release binaries must print the same output.

`frust bench --growth`, `--latency` and `--churn` run the target-growth, edit-latency and configuration-churn
measurements on their own.

## Results

Median of 7 runs from `frust bench --runs 7` on macOS (Apple M4 Max, 16 cores, APFS, rustc 1.99.0), with 12 local crates
plus `serde` and `serde_json`. The machine was not idle (load average 7 before, 9 after), so absolute times carry
noise; ratios are of medians and the spread is in the report.

| Measure | cargo | frust | Result |
|---|---|---|---|
| Release clean rebuild (cache warm) | 4.22s | 352ms | 12.0x faster |
| Debug clean rebuild (cache warm) | 2.64s | 467ms | 5.7x faster |
| Debug build of a second project (cache warm) | 2.72s | 1.11s | 2.5x faster (workspace crates are keyed by their path, so only registry dependencies are shared between projects) |
| Release build of a second project (cache warm) | 4.19s | 2.52s | 1.7x faster |
| Cold build, empty cache (debug / release) | 2.71s / 4.21s | 2.89s / 4.36s | 7% / 4% slower |
| No-op build | 32ms | 38ms | +6 ms |
| Incremental edit of a small project | 113ms | 203ms | slower (each state is stored for the cache) |
| Release binary vs cargo, same target dir | | | byte-identical |
| Release binary vs cargo, different target dir | | | 48 bytes differ (Mach-O UUID and signature, which plain cargo also varies) |
| Release binary run time (median of 20) | 1595us | 1665us | the same within noise, outputs equal |
| Two projects on disk | 273.1 MiB | 235.3 MiB | 14% smaller (37.7 MiB; clones counted once) |
| One target directory after six configuration changes | 321.6 MiB | 85.4 MiB after `frust compact`; 126 MiB with automatic compaction | 73% / 61% smaller |
| 100 edits across crates | 168.2 MiB target, 121.6 MiB incremental | 41.5 MiB target, 23.0 MiB incremental | 75% smaller target; 427 units compiled by each |
| Rebuild after compaction | | | 0 units recompiled, binary output unchanged |
| Eight identical builds at once | | | one build, finishing together (`tests/stress.rs`) |

Maintenance is timed separately: the daily gc pass took 15ms and a full auto-compaction pass 1.06s. Frust's speed
rows run with both switched off.

What is not faster: an uncached compile. rustc sets that floor, and changing flags to beat it would change the
executable. A single fresh project's target directory has the same content as cargo's; it is smaller only through
compression (macOS APFS), removal of superseded variants and clone sharing across projects.


## Performance and growth follow-up, 31 October 2026

Compared the release binaries from `06582b4` with the working-tree changes on Darwin 27 arm64, 16 cores.
The full `frust bench --quick --json` completed all correctness checks, including release byte identity against
both its cold compile and plain Cargo. Its load average changed from 79 to 17; the earlier baseline changed
from 33 to 17. Those runs do **not** establish an overall before/after speed-up: both Cargo and frust varied
substantially with machine load.

Two narrower comparisons alternated the old and new versions:

| Measurement | Before | After | Interpretation |
|---|---:|---:|---|
| Content-validated cache hit, 32 extern paths totalling 158 MiB | 104.3 ms | 31.3 ms | 3.3x faster; 9 runs per version |
| Display-enabled no-op, repository release binaries | 187.8 ms | 136.6 ms | 27% lower median; faster in 9 of 10 pairs |
| Display-off no-op, same project | 172.2 ms | 196.3 ms | No improvement established under load |

The hashing comparison used 32 copies of a valid `serde_core` rlib, a small library compiled once by each
wrapper, then nine verified cache hits per wrapper with `FRUST_CACHE_TRUST_STAMPS=off`. Both compiles produced
identical metadata. This isolates content-hashing throughput; it is not a claim of a 3.3x whole-build speed-up.

The no-op comparison used identical-byte copies of both executables, one warmed `FRUST_HOME`, a shared explicit
`RUSTC_WRAPPER`, the existing local target, and `build --release --bins --locked`. All 44 warm-up and measured
invocations succeeded without compilation or startup-cache misses. Switching wrapper paths otherwise invalidates
Cargo's compiler-information cache. macOS also showed a per-file launch effect: copying the same new executable
reduced time before its first debug line from 105 ms to 13 ms, without changing its bytes.

No-op ranges remained wide: display off was 111–395 ms before and 132–478 ms after; display enabled was
130–281 ms before and 74–186 ms after. For display off, Cargo's own reported median rose from 65 to 95 ms;
wrapper setup was 15.7 versus 17.4 ms and teardown 2.2 versus 1.8 ms. No display-off speed-up is claimed.

The growth portion of the full benchmark retained its disk-space benefit:

| After 30 edits and feature changes | Cargo | frust automatic compaction |
|---|---:|---:|
| Target directory | 97.1 MiB | 31.1 MiB |
| Incremental directory | 51.4 MiB | 12.9 MiB |
| Units compiled over the edit loop | 85 | 85 |
| Units compiled by the final unchanged rebuild | 0 | 0 |

The shared frust cache used another 26.9 MiB. Both binaries printed the same output, and no stale incremental
sessions remained after automatic compaction. Separate regressions verify that unchanged targets are reassessed
for age-based cleanup and that busy profiles are retried, while current outputs and incremental sessions survive.

Validation: 416 native tests passed; strict Clippy passed. Linux and Windows all-target compile checks passed
with `blake3/pure` to avoid requiring cross C toolchains; runtime tests were run on macOS.
