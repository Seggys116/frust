<div align="center">

<img src=".github/assets/hero.svg" alt="fRust" width="880">

**A Rust compiler and Cargo toolchain aimed at faster rebuilds.**

[![CI](https://github.com/Seggys116/frust/actions/workflows/ci.yml/badge.svg)](https://github.com/Seggys116/frust/actions/workflows/ci.yml)
[![License: MIT](https://img.shields.io/badge/license-MIT-ce422b.svg)](https://github.com/Seggys116/frust/blob/main/LICENSE)
[![Rust 1.100](https://img.shields.io/badge/rust-1.100-ffd23f.svg)](https://www.rust-lang.org)
![Platforms](https://img.shields.io/badge/platforms-macOS%20%7C%20Linux-30363d.svg)

[Install](#install) · [How it works](#how-it-works) · [Use](#use) · [Builds that overlap](#builds-that-overlap) · [Docs](https://github.com/Seggys116/frust/blob/main/docs/how-it-works.md)

<img src=".github/assets/divider.svg" alt="" width="880">

</div>

<br>

> fRust installs a complete Rust 1.100.0 toolchain built from pinned upstream sources. The language, the standard
> library, the command lines, flags, stdout, stderr and exit codes are upstream's; the changes are in how Cargo
> stores and reuses work.

<table>
<tr>
<td width="25%" valign="top"><b>Unit cache</b><br><sub>Finished dependency outputs and heavy build script runs are stored by content and restored after verification, so a clean rebuild skips them.</sub></td>
<td width="25%" valign="top"><b>Central targets</b><br><sub>One target directory per workspace and toolchain under a user-configurable root. Projects stay clean, and every explicit Cargo override still wins.</sub></td>
<td width="25%" valign="top"><b>Cooperative builds</b><br><sub>Identical concurrent checks share one compile, and independent work keeps running while another build holds a shared unit.</sub></td>
<td width="25%" valign="top"><b>Managed cleanup</b><br><sub>Superseded executables in managed targets are removed automatically. Test outputs, other binaries and explicit paths are left alone.</sub></td>
</tr>
</table>

## How it works

You run `fcargo`. It is Cargo from the fRust toolchain, with a few patches, and it picks `frustc` as the compiler. Before it compiles a dependency, it looks the unit up in a content-addressed store keyed on the compiler, the arguments, the environment and the inputs. A verified hit restores the outputs without starting the compiler. Anything it cannot verify compiles as usual.

<p align="center"><img src=".github/assets/flow.svg" alt="fcargo resolves, schedules and restores verified cached outputs, frustc analyzes and generates code, the native linker produces the final artifact." width="880"></p>

<p align="center"><img src=".github/assets/divider.svg" alt="" width="880"></p>

## Install

**macOS and Linux**

```sh
cargo install --git https://github.com/Seggys116/frust frust-rs --bin frust-install
frust-install
```

The installer verifies a sha-256 digest before extracting and keeps the previous version. The archive is the whole toolchain: Cargo, the compiler, rustdoc, clippy, rustfmt, rust-analyzer, the offline docs and the standard library for the host and the extra targets listed below. It installs under `~/.local/share/frust` (macOS: `~/Library/Application Support/fRust`); add `current/bin` of that directory to `PATH`. On Linux x86-64 it installs the build for x86-64-v3 when your CPU supports it and the baseline build otherwise; `FRUST_VARIANT=baseline` or `v3` overrides that, and updates keep the installed build (see [configuration](docs/configuration.md#installer-variants)).

<p align="center"><img src=".github/assets/divider.svg" alt="" width="880"></p>

## Use

Use `fcargo` (or `frust`, the same program) wherever you would use `cargo`:

```sh
fcargo build --release
fcargo +nightly test -p mycrate -- --nocapture
fcargo clippy --all-targets
fcargo fmt --check
fcargo build --target wasm32-unknown-unknown
```

| Command | Purpose |
|---|---|
| `fcargo`, `frust` | Cargo, with the unit cache and central targets. `frust where` prints the central target directory of the workspace in the current directory |
| `fclippy` | `cargo clippy` (`cargo-clippy` and `clippy-driver` are in the toolchain) |
| `frustfmt` | `rustfmt` (`cargo-fmt` is in the toolchain, so `fcargo fmt` works) |
| `frustc` | `rustc` |
| `frustdoc` | `rustdoc` |
| `rust-analyzer` | The language server, for editors |
| `frustup` | `frust-install` under the name of the tool it replaces |
| `frust-install update` | Install the newest release |
| `frust-install rollback` | Switch back to the previous version |
| `frust-install uninstall` | Remove the installation |
| `frust-install target add <target>` | Add a standard library for another target |
| `frust-install target list` | List installed and available targets |

Every archive carries the standard library for these targets besides the host. Linux x86-64: `x86_64-unknown-linux-musl`, `aarch64-unknown-linux-gnu`, `wasm32-unknown-unknown` and `wasm32-wasip1`. macOS (Apple Silicon and Intel): both `apple-darwin` targets, the iOS targets (`aarch64-apple-ios`, `aarch64-apple-ios-sim`, `x86_64-apple-ios`, `aarch64-apple-ios-macabi`, `x86_64-apple-ios-macabi`), `wasm32-unknown-unknown` and `wasm32-wasip1`. Any other target of the official channel is one command away, for example `frust-install target add thumbv7em-none-eabihf`. Targets you add are installed again after `frust-install update`. Building for iOS still needs Xcode and its SDK.

`FRUST_UNIT_CACHE=0` turns the unit cache off. Paths, budgets and the other controls are in [configuration](docs/configuration.md).

## Builds that overlap

Eligible `fcargo check` runs in one workspace share compiled units on Linux and macOS, including host proc macros in graphs without build scripts. Independent crates keep building while another request holds a shared dependency, and a failed or cancelled producer releases its claim so waiting builds recover. [Cooperative checks](docs/configuration.md#cooperative-checks)

<p align="center"><img src=".github/assets/divider.svg" alt="" width="880"></p>

## Results

The 21 November Benchmark CI run measured the fRust 0.6.0 toolchain against official Rust 1.100.0 for release builds of ripgrep, syn, regex and fd. The table uses the median of eight CPU-time pairs per scenario and workload, then the geometric mean over workloads. Lower is better and 100% is equal. [Benchmark results and method](docs/benchmarks.md#results-of-the-ci-comparison)

| Scenario | Linux x86-64 | Linux x86-64-v3 | macOS arm64 | macOS x86-64 |
|---|---|---|---|---|
| Clean rebuild | 45.2% | 43.0% | 63.2% | 55.5% |
| No-op rebuild | 89.4% | 90.8% | 107.8% | 95.3% |
| Cold build | 100.0% | 100.1% | 100.1% | 100.6% |
| Edit rebuild | 99.4% | 98.6% | 101.8% | 94.7% |

Clean rebuilds restore dependencies from the unit cache, including unpacked debug objects in the default macOS dev profile. The comparison found no consistent CPU improvement for cold builds or edits. On macOS, no-op results also varied too widely to establish a win. The edit row uses the leaf edit.

Against fRust 0.5.2, wide cooperative checks used less peak RSS on all four platforms. Eligible overlapping proc-macro checks avoided the build-directory wait. Disk write gains depend on the workload, and Intel macOS write volume remains unresolved. CI showed no general reduction in allocated footprint. Caching the default macOS dev artifacts uses additional space. [Resource comparisons](docs/benchmarks.md#changes-from-frust-052)

<p align="center"><img src=".github/assets/divider.svg" alt="" width="880"></p>

## Learn more

[How it works](https://github.com/Seggys116/frust/blob/main/docs/how-it-works.md) &middot; [Configuration](https://github.com/Seggys116/frust/blob/main/docs/configuration.md) &middot; [Benchmarks](https://github.com/Seggys116/frust/blob/main/docs/benchmarks.md) &middot; [Changelog](https://github.com/Seggys116/frust/blob/main/CHANGELOG.md) &middot; [Contributing](https://github.com/Seggys116/frust/blob/main/CONTRIBUTING.md) &middot; [Security](https://github.com/Seggys116/frust/blob/main/SECURITY.md)

<br>

<p align="center"><sub>MIT licensed &middot; &copy; 2026 Zak Noble-Clarke</sub></p>

<div align="center">

<a href="https://www.buymeacoffee.com/seggy116"><img src="https://cdn.buymeacoffee.com/buttons/v2/default-violet.png" alt="Buy me a coffee" height="50" /></a>

</div>
