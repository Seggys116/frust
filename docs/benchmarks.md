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
  clean rebuild with display on (frust only), no-op build and an incremental edit. For release: A and B (cargo
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

`frust bench` generates two identical projects (12 local crates plus `serde` and `serde_json`, release profile with
fat LTO and one codegen unit), builds them with plain cargo and with frust under an isolated state directory, and
cleans up after itself. Figures below are ranges over several runs on macOS (Apple Silicon, APFS, rustc 1.99.0) on a
machine that was often heavily loaded, so absolute times swing; the ratios are what to read.

| Measure | Result |
|---|---|
| Release clean rebuild (cache warm) | 9x to 28x faster than cargo (for example 335ms vs 4.8s) |
| Release build of a second project | 1.7x to 2.8x faster |
| Debug clean rebuild (cache warm) | 2x to 10x faster |
| Debug build of a second project | 0.9x to 3.2x (workspace crates are keyed by their path, so only registry dependencies are shared between projects) |
| 29-member workspace, debug clean rebuild | 1.4x to 3.3x faster; `-p` binary 2.3x to 5.3x |
| Branch switch back to a state built before | 3.7x faster (all units restored) |
| Eight identical builds at once | one build, finishing together (`tests/stress.rs`) |
| Edit of a small project | slower than cargo by about 12% inside cargo (cost of storing each state), plus roughly 3-8 ms of frust startup |
| Cold build, empty cache | no faster than cargo; the wrapper costs under 3% |
| No-op build overhead | +3 ms to +10 ms |
| Release binary vs cargo, same target dir | byte-identical |
| Release binary vs cargo, different target dir | 48 bytes differ (Mach-O UUID and signature, which plain cargo also varies) |
| Release binary run time | the same (median of 20 runs) |
| Two projects on disk | 40% to 47% smaller than cargo (clones shared with the cache, compression) |
| One target directory after six configuration changes | cargo 327 MiB; frust 84.5 MiB after `frust compact`; 113 MiB with automatic compaction |
| Rebuild after compaction | 0 units recompiled, binary output unchanged |

The figures above predate the current benchmark methodology and are being refreshed.

What is not faster: an uncached compile. rustc sets that floor, and changing flags to beat it would change the
executable. A single fresh project's target directory has the same content as cargo's; it is smaller only through
compression (macOS APFS), removal of superseded variants and clone sharing across projects.
