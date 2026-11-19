# Candidate

The stage-2 compiler in `target/compiler-rewrite/toolchains/frust-prelink2` (id `frust-0ef1bc386-prelink2`) contains the change below. `qualification/prove-codegen.sh` passed on that prefix before any candidate timing sample: `LLVM_fatlto` ran for both `frustc` (1.252s) and `rustc` (1.214s) on the codegen probe; fat LTO with `-C save-temps` wrote 1 pre-optimization bitcode file at `codegen-units=1` and 8 at `codegen-units=8` for both compilers; warm incremental and fresh release builds matched `.text` bytes and program output for a leaf body, its revert, a generic plus a trait method, a public function plus a macro, a native input, a linker-only `--defsym`, and a same-size edit whose timestamp was restored. This is a correctness result, not a budget ratio.

The earlier prefix `frust-prelink1` has the compiler change and the previous Cargo. It does not have the content checksum below. `frust` (id `frust-0ef1bc386-central1`) remains the unpatched package.

The change applies only when the running executable's filename is `frustc`. A `rustc` invocation, including the hardlink next to `frustc`, keeps the upstream pipeline. On Linux, `/proc/self/exe` keeps the hardlink name.

Two parts:

1. Fat-LTO pre-link. In `optimize`, a `PreLinkFatLTO` stage invoked as `frustc` passes opt-level 0 to the pre-link pipeline. `config.opt_level` is left alone. The later fat-LTO `run_pass_manager` still uses that requested level, so the `LLVM_fatlto` pipeline still runs. Thin LTO is unchanged. The control ripgrep time-passes row that motivated this split is in `BASELINES.md`: inside the 19.589s `ripgrep` invocation, `LLVM_fatlto` was 13.540s and one `LLVM_passes` row was 5.000s.

2. x86 machine outliner. `LLVMRustCreateTargetMachine` sets `EnableMachineOutliner` and `SupportsDefaultOutlining` for `frustc` on x86 and x86_64 when the codegen opt level is not none. X86 already sets `EnableMachineOutliner` in its target constructor and does not implement `shouldOutlineFromFunctionByDefault` (the default returns false). `TargetDefault` therefore adds the pass and then skips every function. The `frustc` path sets the LLVM option `enable-machine-outliner` to `always`, which uses `X86InstrInfo::isFunctionSafeToOutlineFrom`. Functions that use a red zone are skipped by that check. This is not a `-C llvm-args` flag and it is not PGO, BOLT, or a ThinLTO cache.

3. Content checksums. Cargo uses `FingerprintMethod::Content` only when its rustc path is named `frustc`. After each such compile it stores a Blake3 checksum and length for every dep-info input that rustc did not already checksum. The next build compares those checksums, so a same-size edit is rebuilt when its timestamp is restored. A `rustc` invocation keeps Cargo's mtime check. The diff is `support/patches/0002-frustc-content-fingerprint.patch`.

A later read of the packaged binaries, not a timing sample, checked the fingerprint JSON of a one-file crate. `frust-prelink2` and `frust-o1` record `content` when `RUSTC` is named `frustc`, and `mtime` when it is named `rustc`. The unpatched stage-2 cargo records `mtime`. The central package originally shipped the older cargo (`sha256 68d30c1bb572553e783785f5aefe04a749a7969b9bf8852e598ef5e8e7cd7329`), which also recorded `mtime` for both names. That binary is preserved at `target/compiler-rewrite/preserved/cargo-mtime-68d30c1bb572553e`.

The central `bin/cargo` is now the content-checksum binary already built for those dropped prefixes (`sha256 19da6cf49ae9f14389a0fb07382580c02e872172037edb13147eb9bdcad52507`). It records `content` when `RUSTC` is named `frustc` and `mtime` when it is named `rustc`. The control cargo was not replaced. With the new central cargo, a same-size edit of the parity fixture whose timestamp was restored recompiled, and its `.text` and stdout matched a fresh build (`STAMP_OK 2026-11-19T03:59:06+00:00`, stdout `22 2 4 22 44 37`). The earlier `prove-codegen.sh` run on the old central cargo passed leaf, revert, wide, api, native, and link, then failed only that stamp case: the second build finished in 0.00s without compiling. Fat LTO and the codegen-unit counts in that run had already passed for both `frustc` and `rustc`.

The compiler diff for this dropped candidate is `support/patches/rustc/dropped/0001-frustc-fat-prelink-and-x86-outliner.patch`. `support/apply-rustc-patches.sh` does not apply `dropped/`. `qualification/time-build.sh` exports `RUSTC` to a sibling `frustc` only when that file exists. The control and official bin directories have no `frustc`, so their baselines stay on `rustc`.

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

## Second candidate, proved then dropped

`target/compiler-rewrite/toolchains/frust-o1` (id `frust-0ef1bc386-o1`) is a different change. It does not include the outliner. When the executable name is `frustc` and the stage is `PreLinkFatLTO`, the pre-link pipeline runs at `OptLevel::Less` (pass-builder O1). `config.opt_level` is unchanged, so the fat-LTO `run_pass_manager` still uses the requested level. Thin LTO is unchanged. The diff is `support/patches/rustc/dropped/0002-frustc-o1-fat-prelink.patch`. The apply script does not apply `dropped/`.

`qualification/prove-codegen.sh` passed on this prefix before the probe (`PROVE_OK 2026-11-19T02:00:18+00:00`). Fat LTO ran for `frustc` (1.201s) and `rustc` (1.202s). `-C save-temps` wrote 1 pre-optimization bitcode file at `codegen-units=1` and 8 at `codegen-units=8` for both compilers. Warm and fresh `.text` bytes matched for the leaf, its revert, the wide edit, the public API plus macro, the native input, the linker-only symbol, and the restored-timestamp edit.

One cold build and one clean-target leaf per primary workload were then timed with `qualification/time-build.sh`. These are not 30-pair medians. Targets: `target/compiler-rewrite/baselines/o1/{ripgrep,syn,regex}`. All six exited 0 and the cohorts were clean afterward.

| Workload | Run | Wall | Cargo finished | Max RSS |
| --- | --- | --- | --- | --- |
| ripgrep | cold | 26.12s | 26.12s | 459516 kB |
| ripgrep | leaf | 18.20s | 18.20s | 517420 kB |
| syn | cold | 3.07s | 3.06s | 293096 kB |
| syn | leaf | 1.92s | 1.91s | 295652 kB |
| regex | cold | 5.02s | 5.01s | 350112 kB |
| regex | leaf | 0.33s | 0.32s | 200812 kB |

Leaf ratios against the frozen control medians (ripgrep 19.14s / 495216 kB, syn 1.89s / 290560 kB, regex 0.33s / 195780 kB):

- ripgrep 18.20/19.14 = 0.951. The 0.80 line is 15.31s. This sample is 2.89s short of it.
- syn 1.92/1.89 = 1.016.
- regex 0.33/0.33 = 1.000.
- The three-ratio product is 0.966 and the geometric mean is 0.989. A 30-pair median of samples this close to 1 cannot reach 0.80.
- Leaf RSS ratios are 1.045, 1.018, and 1.026, all inside 1.10. Cold ripgrep 26.12/27.27 = 0.958, inside 1.05.

The pre-link level between O1 and O3 is the wrong lever. O0 remains the dropped RSS failure. No Os/O2/Oz ladder was run after these numbers.

## Why no third compiler change was sampled

A control ripgrep leaf was rebuilt with `-Z time-llvm-passes` (`RUSTC_BOOTSTRAP=1`) into `target/compiler-rewrite/baselines/control/ripgrep-llvm-passes`. The log is `target/compiler-rewrite/logs/control-ripgrep-llvm-passes-z.log`. This is a characterization, not a paired sample and not a rewrite of the frozen baseline. The largest LLVM pass report in that log, the `rg` invocation, totals 18.589s wall. That matches the earlier control split of `LLVM_passes` 5.000s plus `LLVM_fatlto` 13.540s (18.540s).

Inside that report the largest pass is `X86 DAG->DAG Instruction Selection` at 2.490s (13.4%). Backend passes sum to 5.878s (31.6%). Middle-end passes sum to 12.709s (68.4%). The largest middle-end pass is `InlinerPass` at 0.869s (4.7%). Several names appear twice because the pre-link pipeline and the fat-LTO pipeline both run in the one invocation. No pass is as large as the 3.83s gap between the 19.14s control leaf and a 0.80 result.

Closing that gap on ripgrep alone would still leave the three-primary geometric mean at about 0.933 if syn and regex stayed at the ratios above. Reaching 0.80 across all three primaries needs a ratio product of at most 0.512. With syn and regex near 1, ripgrep would have to fall to about 9.8s. That is larger than the whole backend (5.9s). It would require skipping the fat-LTO optimization pipeline. That pipeline is the requested full-LTO optimization, so it stays. A byte-identical reuse also cannot meet the separate 0.97 runtime and stripped-size gates. The 30-pair gate was not run. Thresholds are unchanged. `frust-o1` stays on disk. At that point the active compiler patch series was empty, and the pinned `write.rs` was restored.

The leaf edits themselves sit in the inlining cone. `qualification/edits/regex-leaf.patch` adds a line to `Regex::is_match`, which is already `#[inline]`. `qualification/edits/ripgrep-leaf.patch` adds a line to the generic `search_path`. `qualification/edits/syn-leaf.patch` adds a line to the inner constructor called by public `Error::new`. A green mono-item can keep its pre-optimization IR, but the pre-link pipeline and, for a binary, fat LTO still re-optimize every caller that inlines the changed body. That is the bulk of the leaf time above. Caching unchanged IR does not remove it, and it does not change the emitted machine code, so it cannot move the 0.97 runtime or stripped-size aggregates off 1.

## Flag checks that were not packaged

Two stock-compiler flag checks, not patches and not paired samples, asked whether a code-quality switch could supply the 0.97 size budget without a new invalidation boundary. Both used the unpatched stage-2 `rustc` and the frozen release profile on unmodified ripgrep. The control binary is the differential `rg` (4687944 bytes). `strip --strip-unneeded` makes it 4024992 bytes. Its `.text` is 2538086 bytes. Logs and copies are under `target/compiler-rewrite/logs/char-outliner-rg.log` and `target/compiler-rewrite/char/`.

`-C llvm-args=-enable-machine-outliner=always` at opt-level 3, fat LTO, one codegen unit. Cold wall 26.99s, `/usr/bin/time -v` max RSS 940412 kB, exit 0. The binary has 5228 `OUTLINED_FUNCTION_` symbols. Unstripped 4941920 bytes (1.054 versus the control binary). Stripped 4034368 bytes (1.002 versus 4024992). `.text` fell to 2384230 bytes (0.939). Section growth in `.eh_frame`, `.eh_frame_hdr`, and `.gcc_except_table` used more than the `.text` saving, so the stripped file grew. The size gate rejects growth. This is not the dropped O0 outliner candidate; that one was a compiler patch and was not retimed.

`-C link-arg=-Wl,--icf=all` through `cc -fuse-ld=lld`. Cold wall 26.92s, max RSS 433648 kB, exit 0. Unstripped 4636536 bytes (0.989). Stripped 3973584 bytes (0.987 versus 4024992). `size` text 3752652 versus the control binary's 3804064 (0.986). The stripped ratio is above the 0.97 aggregate, and `--icf=all` may fold address-taken functions, so it was not enabled in the packaged compiler. Neither cold wall is a leaf edit ratio.

On the frozen corpus command, the control `rg` median of 30 runs was 4.84ms. GNU `grep -F -a` on the same file was 0.52ms. The command is not a pure memory-bandwidth ceiling. No LLVM pass in the characterization was shown to turn that gap into a 3% smaller and 3% faster fat-LTO binary while also taking the three leaf ratios' product to 0.512. Nothing from these checks was packaged. Thresholds are unchanged. Checklist items for the measured reuse change and the Linux performance gates stay open.

## Safe identical-code folding

`support/patches/rustc/0001-frustc-safe-icf.patch` is the measured change. When the compiler is invoked as `frustc`, LLD is the linker, and the opt level is 2 or 3, `GccLinker::optimize` passes `--icf=safe` after the existing `-O1` linker flag. `--icf=safe` folds only functions whose address is not taken. The `rustc` hardlink does not pass it. Full LTO and the requested codegen-unit count are unchanged. This is not a cache of whole objects: the linker reuses identical machine code after LTO, and a program can differ from stock when safe duplicates remain.

The stage-2 compiler was rebuilt in `target/compiler-rewrite/build-icf` from a copy of the unpatched control, with `--keep-stage 0` and `--keep-stage-std` 0, 1, and 2. `build-control` was not rebuilt. Its driver is still sha256 `0d5e12331727ef3d4358356bc502e81393ec056aa943e97bd34bac82725dad3d`. The new driver is sha256 `901e3f9adf23a75ffdbbb8df422532c796d895c1c39d9eacc561ad9bf68297f0` (155098784 bytes) and contains the `--icf=safe` string. Recreating the stage-2 sysroot dropped the already-built standard library and `rustdoc`. Those files were copied back from the control stage-2. `libstd-c2e710ac05435005.so` and `rustdoc` match that control. The package is `target/compiler-rewrite/toolchains/frust-icf1`, id `frust-0ef1bc386-icf1`, with the content-checksum cargo (sha256 `19da6cf49ae9f14389a0fb07382580c02e872172037edb13147eb9bdcad52507`). `rustc` and `frustc` are one inode.

A link trace of an opt-level 3 fat-LTO binary showed `frustc` passing `-Wl,--icf=safe` through `cc` to `rust-lld`. The `rustc` hardlink's `cc` argv had no `--icf=safe`. `qualification/prove-codegen.sh` on this prefix recorded `LLVM_FATLTO frustc 1.217000s`, `LLVM_FATLTO rustc 1.207000s`, codegen-unit counts 1 and 8 for both names, and warm/fresh `.text` and stdout parity for the leaf, revert, wide, api, native, linker-only, and restored-timestamp edits. The stamp stdout was `22 2 4 22 44 37`. `PROVE_OK 2026-11-19T04:32:09+00:00`.

One clean-target ripgrep leaf was then timed with `qualification/time-build.sh`. The seed cold build, not a gate sample, was 26.90s wall and 451116 kB max RSS. The leaf was 19.12s wall and 507916 kB max RSS, exit 0. Logs: `target/compiler-rewrite/logs/icf1-ripgrep-cold.log` and `icf1-ripgrep-leaf.log`. Against the frozen leaf medians that is 19.12/19.14 = 0.999 wall and 507916/495216 = 1.026 RSS. A 0.80 leaf would be 15.31s. The `rg` binary is byte-identical to the control differential `rg`: sha256 `805d49a86cf3c51c68ce03e6ef30fc61ba109dcad652de1a0d0125c1a0ebd8b4`, 4687944 bytes unstripped, 4024992 bytes after `strip --strip-unneeded`, `size` text 3804064. The stripped ratio is 1, which misses 0.97. A separate opt-level 3 fat-LTO probe with two identical `#[inline(never)]` functions was also byte-identical between `frustc` and `rustc` (367032 bytes). LLVM already merges identical function bodies before the linker, so `--icf=safe` had nothing left to fold. The earlier unpackaged `--icf=all` check is what changed the ripgrep bytes, to 3973584 stripped (0.987), and it can fold address-taken functions, so it stays out of the compiler. The cohort was clean after the leaf. The 30-pair gate was not run. Thresholds are unchanged. Checklist items 5-7 stay open.
