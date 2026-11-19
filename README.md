<div align="center">

<img src=".github/assets/hero.svg" alt="fRust, a Rust compiler toolchain." width="880">

**A Rust compiler and Cargo toolchain aimed at faster rebuilds.**

[Architecture](docs/how-it-works.md) · [Configuration](docs/configuration.md) · [Benchmarks](docs/benchmarks.md) · [Contributing](CONTRIBUTING.md)

<img src=".github/assets/divider.svg" alt="" width="880">

</div>

## Status

This branch packages a Rust 1.100.0 stage-2 toolchain (upstream commit `0ef1bc386`, pinned in `support/upstream.lock`) with a patched Cargo, `fcargo`, and a `frustc` entry point. Only Linux x86-64 has been built and measured. No compiler change has passed the frozen performance budgets, and no faster-rebuild result is qualified. All performance samples below were taken on 2026-11-19. The host CPU model is not recorded in the qualification files.

- **Packaged toolchain.** Its `rustc`, `frustc`, compiler driver, standard library and rustdoc match an unpatched stage-2 control byte for byte. Its `cargo` is patched: central target directories with a JSON config file, and content checksums for `frustc` builds (`support/patches/`). `qualification/COMPAT.md` records the upstream test suites and the ecosystem comparison. That file is a compatibility record, not a performance result.
- **Kept compiler candidate, not qualified.** `frust-cg1` reuses the machine-code object after fat-LTO optimization when the optimized module is unchanged, and can pack relative relocations (RELR). Both are opt-in in the current patch (`support/patches/rustc/`). One ripgrep leaf sample ran at 0.655 of stock wall time. That edit leaves the optimized module unchanged, so it hits the cache. Single samples of the syn leaf (1.026), regex leaf (1.030) and fd holdout (1.014) were slower than stock. The three-leaf geometric mean is about 0.885 against a 0.80 budget, and the 30-pair gate was not run.
- **Dropped candidates.** A pre-optimization fat-LTO cache passed the full paired Linux gate for the leaf, cold, no-op, tail, memory and disk budgets (the leaf geometric mean of 0.6885 came from one ripgrep cache hit, with syn at 0.98 and regex at 1.00) but failed the holdout, runtime and stripped-size budgets. A two-thread fat-LTO codegen split ran the fd holdout pair at 21.19s against 15.90s with 1.62 times the peak RSS, so the paired gate was not run. Both patches are under `support/patches/rustc/dropped/`, with other rejected candidates. `qualification/CANDIDATE.md` has the numbers and their qualifiers.

| Area | Goal | State |
| --- | --- | --- |
| Compatibility | Build existing Rust projects with the language, standard library and Cargo behavior of a matched upstream release. | Compiler and rustdoc suites run on Linux x86-64. One run-make test failed, and `qualification/COMPAT.md` traces it to the unpatched control build. Cargo's upstream suite was not run. |
| Incremental builds | Reuse unaffected work after a localized edit. | Stock rustc incremental compilation is unchanged. Post-LTO object reuse is the only added reuse, and it is unqualified. |
| Release builds | Shorten release builds while keeping full LTO when requested. | Full LTO optimization still runs in the kept candidate. The leaf, runtime and stripped-size budgets are unmet. |
| Central storage | Keep build outputs under user-configurable central directories by default. | `fcargo` with the patched Cargo places outputs under a per-user target root. The `artifacts` cache directory holds the Cargo unit artifact cache (see below). Its speedup is not measured yet. |

## Toolchain

The toolchain ships `frustc` (a hardlink to `rustc`), `fcargo`, `fcargo-ui`, a patched `cargo` and rustdoc. `fcargo` is a hardlink to the patched `cargo`: it runs Cargo with the central target default directly, and hands over to `fcargo-ui` to draw build progress on a terminal, for static progress, for an explicit `FRUST_CARGO` and for `+toolchain`. That direct entry is compiled on Linux only. `frust-install` downloads and installs the latest published toolchain with its sha256 verified, updates it with `frust-install update`, and rolls back with `frust-install rollback`. It can also install a local archive. `frust-gates` recomputes the frozen budgets from raw paired samples.

Get `frust-install` with `cargo install frust-rs`, then run `frust-install`. The release workflow builds a toolchain for Linux x86-64 and for macOS on Apple Silicon and on Intel, and on any other host the installer reports the missing target. Linux x86-64 is the only verified host. The macOS toolchains are unverified: no macOS toolchain build or measurement is recorded yet, and the workflow's launch check on each macOS runner is the first test they get. The only archive recorded is a local build (`qualification/COMPAT.md`).

<p align="center"><img src=".github/assets/flow.svg" alt="Planned build flow from fcargo through frustc and the native linker, with a central compiler cache. The cache is planned and not implemented." width="880"></p>

## Cargo unit artifact cache

Under `fcargo`, the patched Cargo stores finished rustc outputs of registry, git and directory-source library and proc-macro units, and the output of heavy build script runs of registry and git packages, in a content-addressed store under the artifact cache root. Cargo restores them after verifying checksums and any mismatch compiles normally. Path and workspace members, incremental units and wrapper builds are never cached. libgit2 now starts on first git use instead of on every Cargo invocation. The mechanism, key and limits are in [how it works](docs/how-it-works.md#unit-artifact-cache) and the controls are in [configuration](docs/configuration.md#unit-artifact-cache). The cache is covered by unit tests and a 23-case probe, `qualification/prove-unit-cache.py`, on Linux x86-64 only. The macOS code paths are compiled in and unverified. Build-time results are not recorded yet.

## Native fat-LTO builds

On Linux x86-64, `fcargo build --release` automatically uses incremental compilation and 16 codegen units for eligible local projects that select fat LTO. Explicit profile settings take precedence. See [the configuration details](docs/configuration.md#native-fat-lto-defaults) and [the measured tradeoff](qualification/SEMANTIC-LTO.md#incremental-fat-lto-profile).

## Project history

The [changelog](CHANGELOG.md) records the earlier fRust build accelerator and the transition to the compiler project. See [reporting guidance](SECURITY.md) for security issues.
