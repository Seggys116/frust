<div align="center">

<img src=".github/assets/hero.svg" alt="fRust" width="880">

**A Rust toolchain whose Cargo reuses work it has already verified.**

[![CI](https://github.com/Seggys116/frust/actions/workflows/ci.yml/badge.svg)](https://github.com/Seggys116/frust/actions/workflows/ci.yml)
[![License: MIT](https://img.shields.io/badge/license-MIT-ce422b.svg)](LICENSE)

[Install](#install) · [How it works](#how-it-works) · [Use](#use) · [Docs](docs/how-it-works.md) · [Benchmarks](docs/benchmarks.md) · [Contributing](CONTRIBUTING.md)

<img src=".github/assets/divider.svg" alt="" width="880">

</div>

<br>

<table>
<tr>
<td width="25%" valign="top"><b>Unit cache</b><br><sub>Finished dependency outputs and heavy build script runs are stored by content and restored after verification, so a clean rebuild skips them.</sub></td>
<td width="25%" valign="top"><b>Central targets</b><br><sub>One target directory per workspace and toolchain under a user-configurable root. Every explicit Cargo override still wins.</sub></td>
<td width="25%" valign="top"><b>Cooperative builds</b><br><sub>Identical concurrent checks share one compile instead of waiting on each other.</sub></td>
<td width="25%" valign="top"><b>Managed cleanup</b><br><sub>Superseded executables in managed targets are removed automatically. Test outputs, other binaries and explicit paths are left alone.</sub></td>
</tr>
</table>

## How it works

`fcargo` is Cargo from a Rust 1.100.0 toolchain built from pinned upstream sources, with a few patches. Before it runs `frustc` for a dependency, it looks the unit up in a content-addressed store. A verified hit restores the outputs; anything else compiles as usual.

<p align="center"><img src=".github/assets/flow.svg" alt="fcargo resolves, schedules and restores verified cached outputs, frustc analyzes and generates code, the native linker produces the final artifact." width="880"></p>

## Install

```sh
cargo install --git https://github.com/Seggys116/frust frust-rs --bin frust-install
frust-install
```

The installer verifies a sha-256 digest before extracting and keeps the previous version. Add `~/.local/share/frust/current/bin` to `PATH` (macOS: `~/Library/Application Support/fRust/current/bin`).

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

<p align="center"><img src=".github/assets/divider.svg" alt="" width="880"></p>

The [changelog](CHANGELOG.md) covers the earlier 0.4.x line, which lives on the `legacy-0.4` branch. See [SECURITY.md](SECURITY.md) to report a vulnerability.
