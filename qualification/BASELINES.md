# Measured baselines

These rows were recorded before any candidate compiler sample. The profile is opt-level 3, fat LTO, one codegen unit, incremental on, debug 0, and no strip. No extra `target-cpu` is set. Progress animation is off. The host is Linux x86-64. Wall time is `/usr/bin/time -v`. Cargo's own "Finished" line is listed beside it.

Stock for the paired budgets is the unpatched stage-2 built from the same recipe. Official Rust 1.100.0 is recorded separately because those binaries are PGO builds and this recipe is not.

## Unpatched stage-2 control, ripgrep 15.2.0

Tree `e89fff89ac9af12e8d4ce9d5fd07beb408ca730f`. Package `ripgrep`. Upstream `Cargo.lock` sha256 `7a7d39cda8a03930e578f1dbb724e055771901842eca239e03b01e19da946a64`.

Cold `cargo build --release --locked -p ripgrep`, after a separate fetch of 17 crates:

| Wall | Cargo finished | Max RSS | Exit |
| --- | --- | --- | --- |
| 27.27s | 27.26s | 463216 kB | 0 |

Three leaf incrementals on that same target. Each apply of `qualification/edits/ripgrep-leaf.patch` was reversed after the build.

| Run | Wall | Cargo finished | Max RSS | Exit |
| --- | --- | --- | --- | --- |
| 1 | 19.13s | 19.12s | 517492 kB | 0 |
| 2 | 19.26s | 19.25s | 484640 kB | 0 |
| 3 | 19.14s | 19.13s | 495216 kB | 0 |

## Control ripgrep time-passes

A separate cold build of the same tree used `RUSTFLAGS=-Z time-passes` and `RUSTC_BOOTSTRAP=1`. It is a characterization, not a paired sample. Wall 27.25s, max RSS 443728 kB, exit 0.

The largest single `rustc` invocation, which is the `ripgrep` binary under one codegen unit and fat LTO:

| Pass | Seconds |
| --- | --- |
| total | 19.589 |
| finish_ongoing_codegen | 18.508 |
| LLVM_fatlto | 13.540 |
| LLVM_passes | 5.000 |

Across every crate in that cold build, `LLVM_passes` sums to 17.262s and `LLVM_fatlto` is the single 13.540s row. The fat LTO pipeline is the long part of the incremental rebuild. The pre-link LLVM pipeline is the next slice inside the same invocation.

## Official Rust 1.100.0, ripgrep 15.2.0

Same tree, lock, package, and profile. Compiler `rustc 1.100.0 (0ef1bc386 2026-11-09)`. These binaries are PGO builds, so they are not the paired stock.

Cold `cargo build --release --locked --offline -p ripgrep`:

| Wall | Cargo finished | Max RSS | Exit |
| --- | --- | --- | --- |
| 23.82s | 23.81s | 500172 kB | 0 |

Three leaf incrementals on that same target. Each apply of the leaf patch was reversed.

| Run | Wall | Cargo finished | Max RSS | Exit |
| --- | --- | --- | --- | --- |
| 1 | 17.23s | 17.22s | 535648 kB | 0 |
| 2 | 17.25s | 17.25s | 530824 kB | 0 |
| 3 | 17.53s | 17.52s | 530600 kB | 0 |

## Unpatched stage-2 control, syn 3.0.10

Tree `97ec37dda8a893a460066da694fd1eb7cb30538a`. The package id is `path+file://` plus that tree and `#3.0.10`, because the generated lock also contains registry `syn` 3.0.10 and `syn` 2.0.119 and `-p syn` is ambiguous. Generated `Cargo.lock` sha256 `13af99611da1fe20cfcdd80a64aa7a99e251d0bf7cd6e4740c3cdb7a90c8f59c`.

`cargo build --release --locked --offline` of that library. This build does not link a downstream binary.

| Run | Wall | Cargo finished | Max RSS | Exit |
| --- | --- | --- | --- | --- |
| cold | 3.05s | 3.05s | 292820 kB | 0 |
| leaf 1 | 1.90s | 1.90s | 295712 kB | 0 |

One leaf is not a median. A separate cold time-passes build of the same library finished in 3.06s, wall 3.07s, exit 0. It has no `LLVM_fatlto` row. The largest crate invocation was total 2.501s, of which `LLVM_passes` was 1.430s. Fat LTO of an rlib stops after the pre-link pipeline. The later ripgrep candidate probe is recorded in `qualification/CANDIDATE.md`. It is not a baseline.

## Not measured yet

Control regex and fd, official samples other than ripgrep, and every runtime and size row. Thresholds in `qualification/PROTOCOL.md` are unchanged.
