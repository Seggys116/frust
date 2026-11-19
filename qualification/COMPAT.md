# Compatibility

This records the unpatched packaged toolchain `target/compiler-rewrite/toolchains/frust` (id `frust-0ef1bc386-central1`) against the unpatched stage-2 control from the same recipe. It is not a performance result. The frozen 0.80 / 1.10 budgets remain unmet; see `qualification/CANDIDATE.md`.

The packaged `rustc` stub, `frustc` hardlink, `librustc_driver-c56a332a1933ca87.so`, `libstd-c2e710ac05435005.so`, and `rustdoc` match the stage-2 control byte for byte. After the stamp miss described in `CANDIDATE.md`, packaged `cargo` is the content-checksum binary `sha256 19da6cf49ae9f14389a0fb07382580c02e872172037edb13147eb9bdcad52507`. The control `stage2-tools-bin/cargo` is still `sha256 5a5dc6f6acae82ca2387b0ce36d36c191e2ea9cbeb07702cc168fe1ea3c17e5b` and still records `mtime`. The stage-2 driver was not rewritten by the suite: its mtime stayed `2026-11-19 00:03:38` and its sha256 stayed `0d5e12331727ef3d4358356bc502e81393ec056aa943e97bd34bac82725dad3d`.

## Upstream suites

`./x.py test` used `bootstrap-control.toml`, `--stage 2`, `--keep-stage 1`, `--keep-stage 2`, `--keep-stage-std 1`, `--keep-stage-std 2`, and `--jobs 12`. The first attempt asked for `tests/codegen` and `tests/rustdoc`. Those directories are not steps in this pin. That attempt exited in under a second and is a harness mistake, not a compiler failure.

The corrected compiler run is `target/compiler-rewrite/logs/upstream-suites-2.log`.

| Suite | Result |
| --- | --- |
| ui | 22129 passed, 0 failed, 273 ignored |
| incremental | 180 passed, 0 failed |
| codegen-llvm | 1116 passed, 0 failed, 137 ignored |
| codegen-units | 46 passed, 0 failed |
| run-make | 415 passed, 1 failed, 122 ignored |

`tests/run-make/embed-source-dwarf` panicked because 7 DWARF files had embedded source and the test requires 1. The extra files are `rt.rs`, `backtrace.rs`, `hint.rs`, `common.rs`, `process.rs`, and `function.rs`, plus `main.rs`. Official Rust 1.100.0, compiling the same `main.rs` with `-g -Zembed-source=yes -Cdwarf-version=5`, embeds source only for `main.rs`. Its line table names the standard-library files under `/rustc/0ef1bc386059596dada8fb65b50425aab2ea637a`, which is not a readable path here. The stage-2 line table names the same files under the local checkout, and `-Zembed-source` reads them. The packaged compiler and libstd match the control, so this failure is shared. It is not an fRust-only difference. The test was not skipped or edited.

Rustdoc and the standard library continued in `target/compiler-rewrite/logs/upstream-suites-3.log` and finished with `Build completed successfully`.

| Suite | Result |
| --- | --- |
| rustdoc-ui | 463 passed, 0 failed, 6 ignored |
| rustdoc-json | 186 passed, 0 failed, 8 ignored |
| rustdoc-html | 800 passed, 0 failed, 2 ignored |
| library, including stage1 stdarch-verify | 15407 passed, 0 failed, 184 ignored |

The log of that run contains 70 `test result: ok` lines, 16856 passed, 0 failed, and 200 ignored, which is the rustdoc rows plus the library rows.

Cargo's upstream suite was not started. `x.py test src/tools/cargo` with the control config would rebuild Cargo from the dirty patched submodule into `build-control` and replace the unpatched control cargo. That suite is unverified. macOS is unverified: this host is Linux x86-64 only.

## Locked cohorts

Release builds used the frozen profile (opt-level 3, fat LTO, one codegen unit, incremental, debug 0, strip none), `--locked --offline`, and separate target directories under `target/compiler-rewrite/diff-central`. The cohort trees were clean. This was not a timing sample.

| Artifact | Comparison |
| --- | --- |
| ripgrep `rg` | byte-identical, 4687944 bytes |
| fd `.text` | identical, 2854416 bytes. The full files are the same length and are not byte-identical; one difference is the GNU build-id |
| syn release bitcode | the `.rcgu.o` member and `lib.rmeta` match. The archive member name and `lib.rmeta-link` differ by the codegen-unit filename suffix |
| regex release bitcode | same pattern as syn |

`fd`'s dependency `libregex` and `libsyn` rlibs were byte-identical.

The test run used the same redirected target directories. Failures were the same on both toolchains:

- ripgrep integration: 9 passed, 323 failed. The tests spawn `../rg` next to the test binary. That path does not exist when `CARGO_TARGET_DIR` is outside the cohort. The panic is `NotFound` at `tests/util.rs:335`. Unit tests: 114 passed on both.
- syn: `syn-test-suite` refuses to compile unless `--all-features` or `--features test` is set. Both toolchains hit that check.
- regex library tests: 66 passed, 1 ignored, on both. Doctests did not run: `cargo test` could not execute `rustdoc` because the differential `PATH` did not contain it. That is a harness limit, not a recorded doctest result.
- fd: 165 unit tests passed on both. Integration tests were 122 passed and 1 failed on both (`test_respect_ignore_files`, which printed `gitignored.foo`).

No failure was limited to the packaged toolchain.

## Other checks

Project tests in the fRust repo: 23 passed, 0 failed (2 installer, 5 CLI, 16 core). The central package's `prove-codegen.sh` run passed fat LTO, codegen-unit counts 1 and 8, and the warm/fresh cases other than the stamp. The stamp case passed after the cargo replacement, as recorded in `CANDIDATE.md`.

The local archive `target/compiler-rewrite/archives/frust-0ef1bc386-central1.tar` (`sha256 bb64cbc5aea5142366c4c32bc0cde96a4b75c425e48c25cdd0f7c26637e536be`) is this prefix, including the content-checksum cargo. Installing it on a fresh non-root prefix, running `frustc -Vv`, and building a one-file crate with the installed `frustc` recorded fingerprint `content`. Uninstall removed that tree. Upgrade, rollback, and the interrupted update remain the earlier installer run, which used the previous cargo bytes.

MXFrame's Linux `--features kvm` builds, including `cargo rustc --crate-type staticlib` with `-C opt-level=3 -C lto=fat -C codegen-units=1`, exited 0 on both toolchains. The two staticlibs were byte-identical. The 0.80 gate was not measured on MXFrame. No KVM guest was booted. Apple Silicon `hvf` was not run.
