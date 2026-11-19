<div align="center">

<img src=".github/assets/hero.svg" alt="fRust. CPU time as a fraction of official Rust 1.100.0 on Linux x86-64: clean rebuild of fd 0.02, clean rebuild of ripgrep 0.33, no-op build 0.90, first build with an empty cache 1.02." width="880">

**A Rust toolchain built from pinned upstream sources, with a Cargo that reuses verified work.**

[Architecture](docs/how-it-works.md) · [Configuration](docs/configuration.md) · [Benchmarks](docs/benchmarks.md) · [Contributing](CONTRIBUTING.md)

<img src=".github/assets/divider.svg" alt="" width="880">

</div>

## Status

fRust is at [0.5.0-pre.1](https://github.com/Seggys116/frust/releases/tag/v0.5.0-pre.1), a pre-release for Linux x86-64. It packages Rust 1.100.0 (upstream commit `0ef1bc386`, pinned in `support/upstream.lock`) with an optimized compiler, a patched Cargo, `fcargo`, `frustc` and rustdoc. The compiler, standard library and LLVM library match official Rust 1.100.0; the speedups come from Cargo.

CPU time of fRust divided by official Rust 1.100.0, both on the same four pinned cores with the order alternated, median of three rounds, 19 November 2026:

| Application, profile | First build, empty cache | Clean rebuild | No-op build |
| --- | ---: | ---: | ---: |
| fd, dev | 1.006 | 0.024 | 0.91 |
| oxipng, dev | 1.022 | 0.304 | 0.89 |
| ripgrep, dev | 1.027 | 0.331 | 0.90 |
| ripgrep, release | 1.020 | 0.437 | 0.91 |

A clean rebuild restores dependency units and heavy build script runs from a verified store instead of compiling them. A first build with an empty cache is still 1 to 3 percent slower than official Rust, because the compiler front end binary is slower; the cache bookkeeping costs about 0.1 percent. The full method, raw numbers and the compiler attribution are in [the Linux qualification notes](qualification/OPTIMIZED-LINUX.md).

Not established: edited fat-LTO builds against matched incremental stock, executable runtime and size budgets, the upstream compiler and Cargo test suites against this package, the MXFrame project comparison, and everything on macOS, aarch64 Linux and Windows. The `cargo test` outcomes of ripgrep, fd and oxipng are identical to official Rust with a cold and a restored cache.

## Install

```sh
cargo install --git https://github.com/Seggys116/frust frust-rs --bin frust-install
frust-install install --version 0.5.0-pre.1
export PATH="$HOME/.local/share/frust/current/bin:$PATH"   # macOS: ~/Library/Application Support/fRust/current/bin
fcargo build
```

`frust-install` verifies the sha-256 file before extracting, keeps the previous version, and supports `update`, `rollback` and `uninstall`. Linux x86-64 is the only published target. The prefix defaults to `$FRUST_PREFIX`, then `$XDG_DATA_HOME/frust` or `~/.local/share/frust`.

## What it changes

- **Unit and build script cache.** Under `fcargo`, finished rustc outputs of registry, git and directory-source libraries and proc-macros, and heavy build script runs, are stored in a content-addressed store and restored after verification. Any mismatch compiles normally. Path and workspace members, incremental units and wrapper builds are never cached. See [how it works](docs/how-it-works.md#unit-artifact-cache) and [configuration](docs/configuration.md#unit-artifact-cache).
- **Faster startup.** libgit2 starts on first git use instead of on every Cargo invocation, and `fcargo` is the patched Cargo itself on Linux, so a plain invocation skips a process launch.
- **Central targets.** Build outputs go under a per-user target root, namespaced by toolchain and workspace, with every explicit Cargo override preserved. A JSON file sets the roots. Superseded executables of managed targets are cleaned up by default.
- **Native fat-LTO defaults.** On Linux x86-64, release builds that select fat LTO use incremental compilation and 16 codegen units when the project leaves them open. See [configuration](docs/configuration.md#native-fat-lto-defaults) and [the measured tradeoff](qualification/SEMANTIC-LTO.md#incremental-fat-lto-profile).

Compiler candidates that were measured and not kept are under `support/patches/rustc/` and in [the candidate notes](qualification/CANDIDATE.md).

## Toolchain

The toolchain ships `frustc` (a hardlink to `rustc`), `fcargo` (a hardlink to the patched `cargo`), `fcargo-ui`, `cargo` and rustdoc. `fcargo` hands over to `fcargo-ui` to draw build progress on a terminal, for static progress, for an explicit `FRUST_CARGO` and for `+toolchain`. `frust-gates` recomputes the frozen budgets from raw paired samples.

<p align="center"><img src=".github/assets/flow.svg" alt="Build flow: fcargo resolves, schedules and restores verified cached outputs, frustc analyzes and generates code, the native linker produces the final artifact." width="880"></p>

## Project history

The [changelog](CHANGELOG.md) records the earlier fRust build accelerator (the 0.4.x line, kept on the `legacy-0.4` branch) and the transition to the compiler project. See [reporting guidance](SECURITY.md) for security issues.
