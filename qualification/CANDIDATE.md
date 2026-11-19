# Candidate

The stage-2 compiler in `target/compiler-rewrite/toolchains/frust-prelink2` (id `frust-0ef1bc386-prelink2`) contains the change below. `qualification/prove-codegen.sh` passed on that prefix before any candidate timing sample: `LLVM_fatlto` ran for both `frustc` (1.252s) and `rustc` (1.214s) on the codegen probe; fat LTO with `-C save-temps` wrote 1 pre-optimization bitcode file at `codegen-units=1` and 8 at `codegen-units=8` for both compilers; warm incremental and fresh release builds matched `.text` bytes and program output for a leaf body, its revert, a generic plus a trait method, a public function plus a macro, a native input, a linker-only `--defsym`, and a same-size edit whose timestamp was restored. This is a correctness result, not a budget ratio.

The earlier prefix `frust-prelink1` has the compiler change and the previous Cargo. It does not have the content checksum below. `frust` (id `frust-0ef1bc386-central1`) remains the unpatched package.

The change applies only when the running executable's filename is `frustc`. A `rustc` invocation, including the hardlink next to `frustc`, keeps the upstream pipeline. On Linux, `/proc/self/exe` keeps the hardlink name.

Two parts:

1. Fat-LTO pre-link. In `optimize`, a `PreLinkFatLTO` stage invoked as `frustc` passes opt-level 0 to the pre-link pipeline. `config.opt_level` is left alone. The later fat-LTO `run_pass_manager` still uses that requested level, so the `LLVM_fatlto` pipeline still runs. Thin LTO is unchanged. The control ripgrep time-passes row that motivated this split is in `BASELINES.md`: inside the 19.589s `ripgrep` invocation, `LLVM_fatlto` was 13.540s and one `LLVM_passes` row was 5.000s.

2. x86 machine outliner. `LLVMRustCreateTargetMachine` sets `EnableMachineOutliner` and `SupportsDefaultOutlining` for `frustc` on x86 and x86_64 when the codegen opt level is not none. X86 already sets `EnableMachineOutliner` in its target constructor and does not implement `shouldOutlineFromFunctionByDefault` (the default returns false). `TargetDefault` therefore adds the pass and then skips every function. The `frustc` path sets the LLVM option `enable-machine-outliner` to `always`, which uses `X86InstrInfo::isFunctionSafeToOutlineFrom`. Functions that use a red zone are skipped by that check. This is not a `-C llvm-args` flag and it is not PGO, BOLT, or a ThinLTO cache.

3. Content checksums. Cargo uses `FingerprintMethod::Content` only when its rustc path is named `frustc`. After each such compile it stores a Blake3 checksum and length for every dep-info input that rustc did not already checksum. The next build compares those checksums, so a same-size edit is rebuilt when its timestamp is restored. A `rustc` invocation keeps Cargo's mtime check. The diff is `support/patches/0002-frustc-content-fingerprint.patch`.

The compiler diff is `support/patches/rustc/0001-frustc-fat-prelink-and-x86-outliner.patch`, applied by `support/apply-rustc-patches.sh`. `qualification/time-build.sh` exports `RUSTC` to a sibling `frustc` only when that file exists. The control and official bin directories have no `frustc`, so their baselines stay on `rustc`.

A control time-passes build of the syn library had no `LLVM_fatlto` row. Fat LTO of an rlib stops after the pre-link pipeline, and that pipeline was 1.430s of a 2.501s crate invocation. The skip changes that pre-link copy. A downstream fat-LTO binary still runs `run_pass_manager` at the requested opt level.

## Probe, then dropped

One cold build and one leaf incremental of ripgrep were timed with `qualification/time-build.sh` against `frust-prelink2`, after the correctness proof and before any 30-pair run. The target was `target/compiler-rewrite/baselines/candidate/ripgrep`. Both exited 0 and the cohort was clean afterward. The `rg` binary contains 4951 `OUTLINED_FUNCTION_` symbols; the unpatched control `rg` contains none. Unstripped size was 4912464 bytes versus the control binary's 4687944. Stripped size was not measured.

| Run | Wall | Cargo finished | Max RSS | Exit |
| --- | --- | --- | --- | --- |
| cold | 24.70s | 24.69s | 1000896 kB | 0 |
| leaf 1 | 16.74s | 16.73s | 1044428 kB | 0 |

Against the recorded control medians (cold wall 27.27s and RSS 463216 kB; leaf wall 19.14s, the middle of 19.13/19.14/19.26, and leaf RSS 495216 kB, the middle of 484640/495216/517492):

- Cold wall ratio 24.70/27.27 = 0.906, inside the 1.05 cap.
- Leaf wall ratio 16.74/19.14 = 0.875, above the 0.80 geomean cap. The control leaf spread is 0.13s, and this sample is 1.40s slower than a 0.80 result (15.31s).
- Cold RSS ratio 1000896/463216 = 2.16 and leaf RSS ratio 1044428/495216 = 2.11, both above the 1.10 cap.

A 30-pair median cannot bring a 2× RSS result under 1.10, and this leaf ratio does not leave room for the 0.80 geomean. The full paired gate was not run. The candidate is dropped. Thresholds are unchanged. `frust-prelink2` stays on disk as the measured artifact. The source-equivalent package remains `target/compiler-rewrite/toolchains/frust`.
