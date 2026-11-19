# Measured baselines

These rows were recorded before the next candidate compiler sample. The profile is opt-level 3, fat LTO, one codegen unit, incremental on, debug 0, and no strip. No extra `target-cpu` is set. Progress animation is off. The host is Linux x86-64. Wall time is `/usr/bin/time -v`. Cargo's own "Finished" line is listed beside it. Every row exited 0.

Stock for the paired budgets is the unpatched stage-2 built from the same recipe. Official Rust 1.100.0 is recorded separately because those binaries are PGO builds and this recipe is not.

A leaf, wide, or holdout sample starts from an incremental target of the unmodified tree. Syn and regex leaf medians below are three copies of a cold target, because repeating the same rlib patch in one directory reuses the patched work product. Ripgrep and fd still recompile on every reapplication, so their three samples share one target. A no-op is a later build of the same unmodified sources with no edit in between. The build immediately after `patch -R` is labeled post-revert: the reverse moves file mtimes, so Cargo recompiles. The dropped ripgrep probe is in `qualification/CANDIDATE.md` and is not a row in this file.

## Unpatched stage-2 control, ripgrep 15.2.0

Tree `e89fff89ac9af12e8d4ce9d5fd07beb408ca730f`. Package `ripgrep`. Upstream `Cargo.lock` sha256 `7a7d39cda8a03930e578f1dbb724e055771901842eca239e03b01e19da946a64`.

Cold `cargo build --release --locked -p ripgrep`, after a separate fetch of 17 crates:

| Wall | Cargo finished | Max RSS | Exit |
| --- | --- | --- | --- |
| 27.27s | 27.26s | 463216 kB | 0 |

Three leaf incrementals on that same target. Each apply of `qualification/edits/ripgrep-leaf.patch` was reversed after the build. The leaf edits `crates/core/search.rs` in the `ripgrep` package. Median wall 19.14s. Median max RSS 495216 kB.

| Run | Wall | Cargo finished | Max RSS | Exit |
| --- | --- | --- | --- | --- |
| 1 | 19.13s | 19.12s | 517492 kB | 0 |
| 2 | 19.26s | 19.25s | 484640 kB | 0 |
| 3 | 19.14s | 19.13s | 495216 kB | 0 |

Wide edit of `crates/searcher` on that target. Cargo recompiled `grep-searcher`, `grep-printer`, `grep`, and `ripgrep`.

| Wall | Cargo finished | Max RSS | Exit |
| --- | --- | --- | --- |
| 19.72s | 19.71s | 499480 kB | 0 |

Post-revert rebuild, immediately after that wide reverse:

| Wall | Cargo finished | Max RSS | Exit |
| --- | --- | --- | --- |
| 19.56s | 19.55s | 502448 kB | 0 |

Up-to-date no-op, after a later rebuild of the unmodified tree and with no edit in between. Cargo compiled nothing.

| Wall | Cargo finished | Max RSS | Exit |
| --- | --- | --- | --- |
| 0.02s | 0.02s | 41328 kB | 0 |

## Control ripgrep time-passes

A separate cold build of the same tree used `RUSTFLAGS=-Z time-passes` and `RUSTC_BOOTSTRAP=1`. It is a characterization, not a paired sample. Wall 27.25s, max RSS 443728 kB, exit 0.

The largest single `rustc` invocation, which is the `ripgrep` binary under one codegen unit and fat LTO:

| Pass | Seconds |
| --- | --- |
| total | 19.589 |
| finish_ongoing_codegen | 18.508 |
| LLVM_fatlto | 13.540 |
| LLVM_passes | 5.000 |

Across every crate in that cold build, `LLVM_passes` sums to 17.262s and `LLVM_fatlto` is the single 13.540s row. The fat LTO pipeline is the long part of the incremental rebuild. The pre-link LLVM pipeline is the next slice inside the same invocation. The linker inside that invocation was 0.037s.

## Unpatched stage-2 control, syn 3.0.10

Tree `97ec37dda8a893a460066da694fd1eb7cb30538a`. The package id is `path+file://` plus that tree and `#3.0.10`, because the generated lock also contains registry `syn` 3.0.10 and `syn` 2.0.119 and `-p syn` is ambiguous. Generated `Cargo.lock` sha256 `13af99611da1fe20cfcdd80a64aa7a99e251d0bf7cd6e4740c3cdb7a90c8f59c`.

`cargo build --release --locked --offline` of that library. This build does not link a downstream binary and has no `LLVM_fatlto` row. A separate cold time-passes build finished in 3.06s, wall 3.07s, exit 0. The largest crate invocation was total 2.501s, of which `LLVM_passes` was 1.430s.

Two cold builds:

| Run | Wall | Cargo finished | Max RSS | Exit |
| --- | --- | --- | --- | --- |
| first target | 3.05s | 3.05s | 292820 kB | 0 |
| clean seed | 3.05s | 3.04s | 288784 kB | 0 |

Three leaf misses, each copied from the clean seed before `qualification/edits/syn-leaf.patch`. Median wall 1.89s. Median max RSS 290560 kB.

| Run | Wall | Cargo finished | Max RSS | Exit |
| --- | --- | --- | --- | --- |
| 1 | 1.89s | 1.88s | 290560 kB | 0 |
| 2 | 1.89s | 1.88s | 290700 kB | 0 |
| 3 | 1.89s | 1.88s | 290516 kB | 0 |

The first target's first leaf was 1.90s wall, cargo 1.90s, max RSS 295712 kB. Reapplying that same patch in that directory was 0.33s and then 0.30s. Those two rows are work-product reuse, not edit misses.

Wide edit on the first target: wall 1.94s, cargo 1.93s, max RSS 294680 kB. Post-revert rebuild: wall 1.93s, cargo 1.93s, max RSS 288268 kB. Up-to-date no-op: wall 0.04s, cargo 0.03s, max RSS 55520 kB.

## Unpatched stage-2 control, regex 1.13.2

Tree `4226f214c56e584304520adb9080bcc4c792390e`. Package `regex`. Generated `Cargo.lock` sha256 `6b742a7fb6c35c9f8f7c5de1e11a670cd8b1b51cafbd1415d26c0a41d65191a0`. Library only.

| Run | Wall | Cargo finished | Max RSS | Exit |
| --- | --- | --- | --- | --- |
| first cold | 5.05s | 5.05s | 347772 kB | 0 |
| clean seed | 4.87s | 4.86s | 346032 kB | 0 |

Three leaf misses copied from the clean seed. Median wall 0.33s. Median max RSS 195780 kB.

| Run | Wall | Cargo finished | Max RSS | Exit |
| --- | --- | --- | --- | --- |
| 1 | 0.33s | 0.32s | 195824 kB | 0 |
| 2 | 0.33s | 0.33s | 195648 kB | 0 |
| 3 | 0.33s | 0.32s | 195780 kB | 0 |

On the first target, the first leaf was 0.33s and the next two applications were 0.08s and 0.09s. Wide: wall 0.32s, cargo 0.32s, max RSS 193880 kB. Post-revert: wall 0.33s, cargo 0.33s, max RSS 196172 kB. Up-to-date no-op: wall 0.01s, cargo 0.01s, max RSS 36348 kB.

## Unpatched stage-2 control, fd 10.5.2

Tree `b2a287b3993768e0ec52f0852916917f23fb2a38`. Package `fd-find`, not `fd`. Upstream `Cargo.lock` sha256 `080f85e31279c422c5591ad7092ef93c7171711122c7de2b7aa32b84c5e72ed9`. Holdout only. These rows are not used to choose the compiler change.

Cold:

| Wall | Cargo finished | Max RSS | Exit |
| --- | --- | --- | --- |
| 33.72s | 33.71s | 441532 kB | 0 |

Three holdout incrementals. Median wall 15.90s. Median max RSS 448916 kB.

| Run | Wall | Cargo finished | Max RSS | Exit |
| --- | --- | --- | --- | --- |
| 1 | 15.84s | 15.83s | 483428 kB | 0 |
| 2 | 15.90s | 15.89s | 448916 kB | 0 |
| 3 | 15.99s | 15.98s | 447600 kB | 0 |

Post-revert rebuild: wall 15.70s, cargo 15.69s, max RSS 456452 kB. Up-to-date no-op: wall 0.03s, cargo 0.02s, max RSS 47308 kB.

## Official Rust 1.100.0, ripgrep 15.2.0

Same tree, lock, package, and profile. Compiler `rustc 1.100.0 (0ef1bc386 2026-11-09)`. These binaries are PGO builds, so they are not the paired stock.

Cold:

| Wall | Cargo finished | Max RSS | Exit |
| --- | --- | --- | --- |
| 23.82s | 23.81s | 500172 kB | 0 |

Three leaf incrementals. Median wall 17.25s.

| Run | Wall | Cargo finished | Max RSS | Exit |
| --- | --- | --- | --- | --- |
| 1 | 17.23s | 17.22s | 535648 kB | 0 |
| 2 | 17.25s | 17.25s | 530824 kB | 0 |
| 3 | 17.53s | 17.52s | 530600 kB | 0 |

Wide: wall 17.63s, cargo 17.63s, max RSS 537024 kB. Post-revert: wall 17.65s, cargo 17.64s, max RSS 535384 kB. Up-to-date no-op: wall 0.02s, cargo 0.01s, max RSS 29692 kB.

## Official Rust 1.100.0, syn 3.0.10

Same library package id and lock as the control.

| Run | Wall | Cargo finished | Max RSS | Exit |
| --- | --- | --- | --- | --- |
| cold | 2.60s | 2.59s | 304888 kB | 0 |
| leaf, first application | 1.71s | 1.70s | 308604 kB | 0 |
| leaf, repeat | 0.25s | 0.24s | 231272 kB | 0 |
| leaf, repeat | 0.25s | 0.24s | 230700 kB | 0 |
| wide | 1.73s | 1.72s | 311080 kB | 0 |
| post-revert | 1.72s | 1.71s | 307876 kB | 0 |
| up-to-date no-op | 0.03s | 0.03s | 44752 kB | 0 |

The repeated leaves reused the patched work product. The first leaf is the edit.

## Official Rust 1.100.0, regex 1.13.2

| Run | Wall | Cargo finished | Max RSS | Exit |
| --- | --- | --- | --- | --- |
| cold | 4.24s | 4.24s | 358832 kB | 0 |
| leaf, first application | 0.30s | 0.29s | 208480 kB | 0 |
| leaf, repeat | 0.07s | 0.06s | 158220 kB | 0 |
| leaf, repeat | 0.07s | 0.07s | 158172 kB | 0 |
| wide | 0.28s | 0.28s | 207092 kB | 0 |
| post-revert | 0.29s | 0.28s | 208776 kB | 0 |
| up-to-date no-op | 0.01s | 0.01s | 25788 kB | 0 |

## Official Rust 1.100.0, fd 10.5.2

Cold: wall 34.12s, cargo 34.10s, max RSS 504416 kB.

Three holdouts. Median wall 14.20s.

| Run | Wall | Cargo finished | Max RSS | Exit |
| --- | --- | --- | --- | --- |
| 1 | 14.22s | 14.21s | 526676 kB | 0 |
| 2 | 14.15s | 14.14s | 523276 kB | 0 |
| 3 | 14.20s | 14.20s | 522008 kB | 0 |

Post-revert: wall 14.14s, cargo 14.13s, max RSS 525960 kB. Up-to-date no-op: wall 0.03s, cargo 0.03s, max RSS 36288 kB.

## Not measured yet

Runtime and stripped size. Thresholds in `qualification/PROTOCOL.md` are unchanged.
