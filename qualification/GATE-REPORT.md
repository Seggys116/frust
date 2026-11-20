# Gate report, 20 November 2026

No candidate is kept. No fRust compiler passed every frozen Linux budget. The thresholds in `crates/frust-core/src/gates.rs` (sha256 `6aec6f5c1cb23205083380627db936f82ccd35425777dd4f91185eaef28ecacd`) were not changed. Nothing in this report was pushed or published. macOS arm64 was not run: this host is Linux x86-64, and there is no `aarch64-apple-darwin` runner here.

The judge is the shipped `frust-gates` binary. It exits 1 when any gate is not `passed`. Raw paired samples and the judge JSON live under the git-ignored build directory `target/compiler-rewrite/logs/`. A clone of this repository cannot recompute them.

## Full paired judge

The only full paired judge on 20 November is the interface-hash cutoff, prefix `toolchains/frust-0ef1bc386-cutoff1`, report `target/compiler-rewrite/logs/cutoff-gates.json`, sha256 `7bf8d360b8d72e684f516189ba8ea84237c44aa07f75112f113d08ef13713a7a`. The judge exited 1. `FRUST_INTERFACE_CUTOFF=1` was set on the fRust side only. The three application binaries were byte-identical to stock (ripgrep 4024992, oxipng 1338440, rustfmt port 3835192), so the per-workload runtime bound is waived and the runtime aggregate stays at 1.

| Gate | Status | Figure |
| --- | --- | --- |
| edited-release-build | passed | geomean 0.68822331, upper 0.68958244 |
| holdout-build | passed | median 0.21986888 |
| cold-build | passed | median 1.00599739 |
| noop-overhead | passed | -0.00064504 s, allowance 0.02000000 s |
| tail-latency | passed | syn/leaf p95 1.01924986, every primary and wide series at or under 1.05 |
| peak-memory | passed | max 1.06854079 |
| disk-bytes | passed | max 1.09510395 |
| runtime-workloads | passed | identical code waives the 1.01 upper |
| runtime-aggregate | failed | geomean 0.99844853, upper 1.00384281 |
| stripped-size | failed | geomean 1.00000000 |

The build-time passes belong to that cutoff compiler. They are not copied onto any other compiler. The cutoff is not the default toolchain. Identical emitted code cannot meet the 0.97 runtime or stripped-size aggregates.

## Later probes that are not a full candidate

Each of these kept full LTO, opt-level 3, and one codegen unit. None ran the 630-pair build cohort, so the seven build-time gates stay failed for them. None is packaged as the default compiler.

The size-passing stack is hotfreq16 (ext-TSP, `-hot-callsite-rel-freq=16`, locally-hot budget 525, `-Wl,-z,pack-relative-relocs`, Clang 21 for oxipng's libdeflate). `logs/hotfreq16-gates.json` sha256 `6cbb18daffd00971850868869166cbfe22da8dd4e348e334daa130505460f841`. Runtime geomean 0.99074132, upper 0.99712344, median product 0.97248031. Stripped-size geomean 0.95306482 passed. The 0.97 runtime upper needs a median product at or below 0.912673, and a still lower product once the upper sits above the geomean. RELR stays opt-in.

A Clang 21 plugin, `support/wordscan/`, widens the `lz_extend` loop and is not wired into `frustc` or Cargo. On the eight frozen PNGs the level-12 scan/scalar median was 1.006237, and `deflate_compress.o` grew. An inlined AVX2 form of that same loop, compiled into a throwaway copy of libdeflate with `-mavx2` so the compare is not a call, was timed on the eight inflated IDAT buffers (196864 bytes each). Level-12 compressed bytes matched Clang `-O3` (1167677). Two 30-pair medians, nice 15, no CPU pin, host load about 15, were 0.974152 and 0.974558 against `-O3`, and 0.969262 and 0.964652 against the same file built `-mavx2` without the loop change. The interquartile range crossed 1. That is not the oxipng wall cut (about 8%) a size-passing stack still needs. Applying the 0.974152 factor to the whole hotfreq16 oxipng median would leave the three-workload runtime product at 0.9473. `deflate_compress.o` was 80312 bytes at `-O3`, 89112 at `-mavx2`, and 90808 with the inlined loop. `frust-gates` was not run. Detail is in `target/compiler-rewrite/logs/wordscan.md`.

## CLI progress

`cargo +1.100.0 test --workspace --locked` from `crates/` is the shipped check for config rejection, progress marks, opaque-input misses, and the installer lifecycle. A rerun on this host exited 0: `frust_cli` 22, `fcargo` 37, `tests/installer.rs` 37, `tests/release.rs` 13, `tests/tool_cli.rs` 6, and `frust_core` 78, which is 193 passed and 0 failed. The `frust-gates` and `frust-install` binary targets and both doc-test targets ran and had no tests of their own. The test names include `modes_follow_progress_and_term`, `frames_use_marks_and_only_stable_totals`, `plain_and_dumb_match_cargo_and_static_keeps_diagnostics`, `malformed_config_names_the_problem`, `hit_requires_ordered_match_and_opaque_inputs_miss`, `install_upgrade_rollback_uninstall_and_interrupted_update`, and `the_binary_installs_rolls_back_and_uninstalls`.

## Installer lifecycle

The shipped `frust-install` binary, on a non-root prefix and three small ustar trees, installed, refused a bad checksum without changing the active tree, staged a third archive without activating it, upgraded, rolled back, and uninstalled twice (`LIFECYCLE_OK 2026-11-20T08:05:45Z`). That run did not install `archives/frust-0ef1bc386-central1.tar`. The log is `target/compiler-rewrite/logs/cli-installer.md`.

## MXFrame

Isolated source snapshot of `/home/seggy116/Projects/mx-frame` at `a7e6ef9db6312afbe517be33fd3d129294f5c3ba` on `x86_64-KVM`. Private source digest `a58c1648938ce3962fe50296a24bcc69ce1562ef04bd5e3f709d3a86b673e186` (4316 files). Control and `frust-span1` each built `--features kvm` at opt-level 3 and codegen-units 1, with and without incremental, and each produced a fat-LTO staticlib. The two staticlibs matched, sha256 `1d8aa9284af6fbdb5a0571ff35a2cda6dc8d39f6f2769a4766fd5518eaf6218d`, 146562158 bytes. A snapshot-only edit of `parse_hex_or_decimal` changed both cdylibs to the same new hash and failed `abi::symbols::tests::addresses_parse_as_hex_or_decimal`. Reverting the file restored the previous hash and a passing test. The snapshot was removed. A second inventory matched the first. No KVM guest was booted. This is a compiler success for the isolated build and edit/revert, not a pass of the Linux performance budgets. Detail is in `target/compiler-rewrite/logs/mxframe-summary.txt`.

## What remains failed

Runtime aggregate and stripped size on the full paired judge. Every later codegen probe either missed the 0.97 runtime upper, missed stripped size, or both, or was not a compiler change. macOS install and execution are unverified.
