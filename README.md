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

The installer verifies a sha-256 digest before extracting and keeps the previous version. It installs under `~/.local/share/frust` (macOS: `~/Library/Application Support/fRust`); add `current/bin` of that directory to `PATH`. On Linux x86-64 it installs the build for x86-64-v3 when your CPU supports it and the baseline build otherwise; `FRUST_VARIANT=baseline` or `v3` overrides that, and updates keep the installed build (see [configuration](docs/configuration.md#installer-variants)).

<p align="center"><img src=".github/assets/divider.svg" alt="" width="880"></p>

## Use

Use `fcargo` wherever you would use `cargo`:

```sh
fcargo build --release
fcargo +nightly test -p mycrate -- --nocapture
```

| Command | Purpose |
|---|---|
| `frust-install update` | Install the newest release |
| `frust-install rollback` | Switch back to the previous version |
| `frust-install uninstall` | Remove the installation |

`FRUST_UNIT_CACHE=0` turns the unit cache off. Paths, budgets and the other controls are in [configuration](docs/configuration.md).

## Builds that overlap

Identical `fcargo check` runs in one workspace share a single compile of each unit instead of queueing behind each other. Independent crates keep building while another request holds a shared dependency, and a failed or cancelled producer releases its claim so waiting builds recover.

<p align="center"><img src=".github/assets/divider.svg" alt="" width="880"></p>

## Learn more

[How it works](https://github.com/Seggys116/frust/blob/main/docs/how-it-works.md) &middot; [Configuration](https://github.com/Seggys116/frust/blob/main/docs/configuration.md) &middot; [Benchmarks](https://github.com/Seggys116/frust/blob/main/docs/benchmarks.md) &middot; [Changelog](https://github.com/Seggys116/frust/blob/main/CHANGELOG.md) &middot; [Contributing](https://github.com/Seggys116/frust/blob/main/CONTRIBUTING.md) &middot; [Security](https://github.com/Seggys116/frust/blob/main/SECURITY.md)

<br>

<p align="center"><sub>MIT licensed &middot; &copy; 2026 Zak Noble-Clarke</sub></p>

<div align="center">

<a href="https://www.buymeacoffee.com/seggy116"><img src="https://cdn.buymeacoffee.com/buttons/v2/default-violet.png" alt="Buy me a coffee" height="50" /></a>

</div>
