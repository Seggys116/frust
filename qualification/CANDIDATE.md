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

`support/patches/rustc/0001-frustc-safe-icf.patch` was the measured change. When the compiler was invoked as `frustc`, LLD was the linker, and the opt level was 2 or 3, `GccLinker::optimize` passed `--icf=safe` after the existing `-O1` linker flag. `--icf=safe` folds only functions whose address is not taken. The `rustc` hardlink did not pass it. Full LTO and the requested codegen-unit count were unchanged. This was not a cache of whole objects: the linker reused identical machine code after LTO, and a program could differ from stock when safe duplicates remained.

The stage-2 compiler was rebuilt in `target/compiler-rewrite/build-icf` from a copy of the unpatched control, with `--keep-stage 0` and `--keep-stage-std` 0, 1, and 2. `build-control` was not rebuilt. Its driver is still sha256 `0d5e12331727ef3d4358356bc502e81393ec056aa943e97bd34bac82725dad3d`. The new driver is sha256 `901e3f9adf23a75ffdbbb8df422532c796d895c1c39d9eacc561ad9bf68297f0` (155098784 bytes) and contains the `--icf=safe` string. Recreating the stage-2 sysroot dropped the already-built standard library and `rustdoc`. Those files were copied back from the control stage-2. `libstd-c2e710ac05435005.so` and `rustdoc` match that control. The package is `target/compiler-rewrite/toolchains/frust-icf1`, id `frust-0ef1bc386-icf1`, with the content-checksum cargo (sha256 `19da6cf49ae9f14389a0fb07382580c02e872172037edb13147eb9bdcad52507`). `rustc` and `frustc` are one inode.

A link trace of an opt-level 3 fat-LTO binary showed `frustc` passing `-Wl,--icf=safe` through `cc` to `rust-lld`. The `rustc` hardlink's `cc` argv had no `--icf=safe`. `qualification/prove-codegen.sh` on this prefix recorded `LLVM_FATLTO frustc 1.217000s`, `LLVM_FATLTO rustc 1.207000s`, codegen-unit counts 1 and 8 for both names, and warm/fresh `.text` and stdout parity for the leaf, revert, wide, api, native, linker-only, and restored-timestamp edits. The stamp stdout was `22 2 4 22 44 37`. `PROVE_OK 2026-11-19T04:32:09+00:00`.

One clean-target ripgrep leaf was then timed with `qualification/time-build.sh`. The seed cold build, not a gate sample, was 26.90s wall and 451116 kB max RSS. The leaf was 19.12s wall and 507916 kB max RSS, exit 0. Logs: `target/compiler-rewrite/logs/icf1-ripgrep-cold.log` and `icf1-ripgrep-leaf.log`. Against the frozen leaf medians that is 19.12/19.14 = 0.999 wall and 507916/495216 = 1.026 RSS. A 0.80 leaf would be 15.31s. The `rg` binary is byte-identical to the control differential `rg`: sha256 `805d49a86cf3c51c68ce03e6ef30fc61ba109dcad652de1a0d0125c1a0ebd8b4`, 4687944 bytes unstripped, 4024992 bytes after `strip --strip-unneeded`, `size` text 3804064. The stripped ratio is 1, which misses 0.97. A separate opt-level 3 fat-LTO probe with two identical `#[inline(never)]` functions was also byte-identical between `frustc` and `rustc` (367032 bytes). LLVM already merges identical function bodies before the linker, so `--icf=safe` had nothing left to fold. The earlier unpackaged `--icf=all` check is what changed the ripgrep bytes, to 3973584 stripped (0.987), and it can fold address-taken functions, so it stays out of the compiler. The cohort was clean after the leaf. The 30-pair gate was not run. Thresholds are unchanged. Checklist items 5-7 stay open.

The candidate is dropped. The patch was reversed on the stage-2 checkout and moved to `support/patches/rustc/dropped/0003-frustc-safe-icf.patch`. At that point `linker.rs` did not contain `invoked_as_frustc` or `--icf=safe`, and `support/apply-rustc-patches.sh` reported no rustc patches. `target/compiler-rewrite/toolchains/frust-icf1`, `parity/frust-0ef1bc386-icf1`, and `probes/icf1` stay on disk and were not retimed. `build-control` was not modified. `build-icf` was later reused for the successor below, which replaced that stage-2 driver.

## Default unwind tables are not a 3% size cut

A stock stage-2 cold build of unmodified ripgrep with `-C force-unwind-tables=no`, still at opt-level 3, fat LTO, and one codegen unit, asked whether Linux `default_uwtable` was the 0.97 size budget. It is a characterization, not a paired sample and not a candidate. Cold wall 26.96s, max RSS 438060 kB, exit 0. Log: `target/compiler-rewrite/logs/char-unwind-no-rg.log`. The binary is `target/compiler-rewrite/char/unwind-no2/release/rg` (4668920 bytes, sha256 `019abe07e12e124bc1b03356319776d7553dc2cc7ff0a7349d20424875a96d52`). `.text` stayed 2538086 bytes. `.eh_frame` fell from 210780 to 196836 and `.eh_frame_hdr` from 31876 to 26796. `strip --strip-unneeded` left 4005968 bytes, a 19024-byte saving against 4024992 (ratio 0.995). The 0.97 bar is 3904242 bytes, which needs a saving of at least 120750. LLVM still emits unwind information for functions that are not `nounwind`, so turning off the default `uwtable` attribute does not remove the bulk of `.eh_frame`. This flag was not added to the compiler.

## Post-LTO codegen reuse and RELR

`support/patches/rustc/0001-frustc-codegen-reuse.patch` is the active compiler change. `support/apply-rustc-patches.sh` reports it already applied. It is not `--icf=safe`, not a pre-link opt-level change, and not ThinLTO. Two `frustc`-only pieces:

1. After fat-LTO optimization, and only when emitting one object file, hash the optimized module and reuse a previous object with the same hash. The hash is order-independent. `StructuralHash` does not cover global initializers, so those are hashed separately, along with linkage, function attributes, constant metadata such as `!range`, the data layout, the target triple, and module flags. Debug-info nodes are not in the hash. If the module has `llvm.dbg.cu`, the object is compiled and not stored. The cache file lives next to the object, under `frust-codegen-cache/`, so it survives the per-invocation temporary name. A miss compiles normally and then stores the object. Full LTO still runs: the hash is taken after `run_pass_manager`. Pre-optimization bitcode is still written, so the codegen-unit count is unchanged. The `rustc` hardlink does not hash or reuse.

2. At opt-level 2 or 3, on a GNU ELF link that is not `crt-static` and not WebAssembly, pass `-z pack-relative-relocs`. `--pack-dyn-relocs=relr` packs the relocations but does not add the `GLIBC_ABI_DT_RELR` version, and the loader then rejects the file. The flag that loads is `-z pack-relative-relocs`.

The stage-2 compiler was rebuilt in `target/compiler-rewrite/build-icf` with `--keep-stage 0` and `--keep-stage-std` 0, 1, and 2. `build-control` was not rebuilt. Recreating the stage-2 sysroot again dropped `rustdoc` and the standard library; those 54 files were copied back from the control stage-2. `libstd-c2e710ac05435005.so` and `rustdoc` match that control. The new driver is sha256 `15c710071f3babdba3ddf16a0e654fa5c9f8c43214e0f7ce0ea87825ebe8f1a4` (155149824 bytes). It contains `pack-relative-relocs` and `frust-codegen-cache`, and it does not contain `--icf=safe`. The package is `target/compiler-rewrite/toolchains/frust-cg1`, id `frust-0ef1bc386-cgcache1`, with the content-checksum cargo (sha256 `19da6cf49ae9f14389a0fb07382580c02e872172037edb13147eb9bdcad52507`). `rustc` and `frustc` are one inode. `toolchains/frust` and `toolchains/frust-icf1` were not overwritten.

`qualification/prove-codegen.sh` recorded `LLVM_FATLTO frustc 1.232000s`, `LLVM_FATLTO rustc 1.212000s`, codegen-unit counts 1 and 8 for both names, and warm/fresh `.text` and stdout parity for the leaf, revert, wide, api, native, linker-only, and restored-timestamp edits. The stamp stdout was `22 2 4 22 44 37`. `PROVE_OK 2026-11-19T05:19:38+00:00`.

The ripgrep leaf's optimized module does not change under the frozen edit. An order-sensitive module hash misses it, because 38 functions are permuted and a raw bitcode hash also misses, but every function body and every global initializer matches. An order-independent hash of those bodies and initializers matches, and it changes when an instruction constant or a global initializer changes. Printed instruction metadata does not match across that pair, so it is not part of the key. That is why codegen can be skipped for this leaf without skipping fat-LTO optimization. Parallel post-LTO codegen was not used: an 8-way split of the optimized bitcode saved about 3.2s after the split itself and pushed the summed resident set over the 1.10 cap.

One clean target was seeded with an unmodified ripgrep release build, then one leaf was timed with `qualification/time-build.sh`. The seed is not a gate sample: 27.17s wall and 442592 kB max RSS, next to the frozen cold median of 27.27s. It left 61 files in `frust-codegen-cache`. The leaf did not add a cache file. Logs: `target/compiler-rewrite/logs/cgcache1-ripgrep-cold.log` and `cgcache1-ripgrep-leaf.log`. The target is `target/compiler-rewrite/probes/cgcache1/ripgrep`. The cohort was clean afterward.

| Run | Wall | Max RSS | Exit |
| --- | --- | --- | --- |
| seed cold | 27.17s | 442592 kB | 0 |
| leaf | 12.53s | 447136 kB | 0 |

Against the frozen leaf medians (wall 19.14s, RSS 495216 kB):

- Leaf wall 12.53/19.14 = 0.655, under the 0.80 bar of 15.31s.
- Leaf RSS 447136/495216 = 0.903, under the 1.10 bar of about 544738 kB.
- Unstripped `rg` is 4405552 bytes, sha256 `9af9ac5e16737ca1da64263aab4c718cae9b00b4cc6f3b3b4f7885ac1f8df866`. `strip --strip-unneeded` leaves 3742608 bytes, 3742608/4024992 = 0.930, under the 0.97 bar of 3904242 bytes. `readelf -V` shows `GLIBC_ABI_DT_RELR`. `rg --version` prints ripgrep 15.2.0.

`.text` is still 2538086 bytes. It is not byte-identical to the control differential `rg`: 12146 bytes differ. The corpus command (`rg --no-config -j 1 -F frust_needle_7 search.txt`) produced the same stdout from both binaries (423040 bytes, exit 0). On 30 runs after 5 warmups, in the same session, the control median was 0.005820s and the candidate median was 0.005766s, a ratio of 0.991. That is not under 0.97. The frozen corpus median of 0.004839s is not the comparison, because the control binary on this session was 0.005820s. A 30-pair build gate would time this same binary. It cannot turn 0.991 into a runtime aggregate of 0.97. The 30-pair gate was not run. Thresholds are unchanged. Checklist items 5-7 stay open. Syn and regex were not sampled.

## Arrow Lake scheduling is not a runtime win

This was a characterization on the unpatched stage-2 compiler, not a patch and not a packaged candidate. `-Z tune-cpu=arrowlake` sets LLVM's scheduling model. It does not change `target-cpu`. A small probe compiled three ways at opt-level 3 with fat LTO: the default and the arrowlake tune both had no AVX or YMM instructions, while `target-cpu=native` had 1330. The same check on the ripgrep binaries below counted 1012 YMM lines in each, so the tune did not widen the ISA. `build-control` was not modified.

Both ripgrep builds used the frozen release profile (opt-level 3, fat LTO, one codegen unit, incremental, debug 0, strip none) from an empty target. They ran together, so the wall times are not cold-gate samples: default 32.57s and 446940 kB, arrowlake 32.84s and 459544 kB, both exit 0. Logs: `target/compiler-rewrite/logs/tune1-rg-base.log` and `tune1-rg-arrow.log`. The default binary matches the control differential `rg` byte for byte, sha256 `805d49a86cf3c51c68ce03e6ef30fc61ba109dcad652de1a0d0125c1a0ebd8b4`, 4687944 bytes. The tuned binary is sha256 `8c57f5500d7108b66c3980c7d8bd8e0b53766510d9013ce57074f7fa5ad2a41a`, 4738152 bytes. `strip --strip-unneeded` leaves 4024992 and 4074944 bytes. 4074944/4024992 = 1.012, so this workload's stripped file grew. The size gate requires every workload to be no larger than stock.

The corpus command was `rg --no-config -j 1 -F frust_needle_7 search.txt`. Stdout matched, 423040 bytes. A first 30-run pair was taken while the rustfmt attempt below was still compiling, and it is not the result: the medians were 0.007968s and 0.008276s. After that load ended, 30 interleaved runs with the order rotated gave:

| Binary | Median | Mean | Population stdev |
| --- | --- | --- | --- |
| default | 0.006181s | 0.006182s | 0.000075s |
| arrowlake | 0.006230s | 0.006223s | 0.000057s |
| control `rg` | 0.006088s | 0.006094s | 0.000074s |

The tuned median over the default median is 0.006230/0.006181 = 1.008. The default binary and `target/compiler-rewrite/char/rg-control` are the same bytes, and those two medians still differ by 1.015, so a ratio this close to 1 is inside the noise of a 6ms process start. It is not a 0.97 runtime win. One workload at about 1.0 cannot be pulled under a 0.97 aggregate by the other two unless those two average about 0.94, and this workload's stripped file already grew.

The locked rustfmt tree (`cebab3e99259be82ff069e5ae89e91855d79e534`, the peel of tag `5eb206859397a1cb7a6ef91790d38591c0a36ac9`) does not compile with this stage-2 compiler. Cargo got past the missing `overload` crate once the network retry ran, then `rustfmt-nightly` failed with 43 errors against the current `rustc_ast` (`EXIT:101` in `tune1-rf-base.log` and `tune1-rf-arrow.log`). The compiler tree's own rustfmt is 1.11.0 (`d957b2ec4b0120979585316f2f342400ccafb16c`) and was not substituted. Oxipng was not built: the ripgrep size regression already fails the per-workload size rule. The tune was not added to `frustc`. The 30-pair gate was not run. Thresholds are unchanged. Checklist items 5-7 stay open.
