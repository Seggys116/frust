# Measured results

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

What is not faster: an uncached compile. rustc sets that floor, and changing flags to beat it would change the
executable. A single fresh project's target directory has the same content as cargo's; it is smaller only through
compression (macOS APFS), removal of superseded variants and clone sharing across projects.
