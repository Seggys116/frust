<div align="center">

<img src="https://raw.githubusercontent.com/Seggys116/frust/main/.github/assets/hero.svg" alt="frust: pixel-art wordmark above an animated build progress bar" width="880">

**A thin accelerator that sits on top of the Rust toolchain you already have.**

[![CI](https://github.com/Seggys116/frust/actions/workflows/ci.yml/badge.svg)](https://github.com/Seggys116/frust/actions/workflows/ci.yml)
[![License: MIT](https://img.shields.io/badge/license-MIT-ce422b.svg)](https://github.com/Seggys116/frust/blob/main/LICENSE)
[![Rust 1.89+](https://img.shields.io/badge/rust-1.89%2B-ffd23f.svg)](https://www.rust-lang.org)
![Platforms](https://img.shields.io/badge/platforms-macOS%20%7C%20Linux%20%7C%20Windows-30363d.svg)

[Install](#install) · [How it works](#how-it-works) · [Use](#use) · [Builds that overlap](#builds-that-overlap) · [Results](#results) · [Docs](https://github.com/Seggys116/frust/blob/main/docs/how-it-works.md)

<img src="https://raw.githubusercontent.com/Seggys116/frust/main/.github/assets/divider.svg" alt="" width="880">

</div>

<br>

> frust does not ship a compiler, patch cargo or re-implement its command line. It asks your installed `cargo` and
> `rustc` what they are, then wraps them. Every argument, `+toolchain`, stdout, stderr and exit code passes through.

<table>
<tr>
<td width="25%" valign="top"><b>Compile cache</b><br><sub>Shared across projects and clean rebuilds. A hit restores the exact outputs without starting rustc, as a copy-on-write clone where the filesystem supports it.</sub></td>
<td width="25%" valign="top"><b>Central targets</b><br><sub>One directory per workspace and toolchain. Projects stay clean and switching toolchains never invalidates the others.</sub></td>
<td width="25%" valign="top"><b>Build locks</b><br><sub>A fair queue in front of cargo's own. Identical concurrent builds share one build, and waiters see who holds the lock.</sub></td>
<td width="25%" valign="top"><b>Clean display</b><br><sub>Colour, live progress, cache counters and terminal integration. Plain cargo output when piped.</sub></td>
</tr>
</table>

## How it works

You run `fcargo`. frust takes the build lock, points cargo at a central target directory and starts your own cargo, which
runs `frust-wrap` in place of rustc for each crate. A cache hit restores the stored outputs without starting the compiler.

<p align="center"><img src="https://raw.githubusercontent.com/Seggys116/frust/main/.github/assets/flow.svg" alt="cargo calls frust-wrap, which looks up a content-hashed key: a hit restores outputs without running rustc, a miss runs rustc and stores the result" width="880"></p>

<p align="center"><img src="https://raw.githubusercontent.com/Seggys116/frust/main/.github/assets/divider.svg" alt="" width="880"></p>

## Install

**macOS and Linux**

```sh
curl -fsSL https://raw.githubusercontent.com/Seggys116/frust/main/install.sh | sh
```

**Windows (PowerShell)**

```powershell
irm https://raw.githubusercontent.com/Seggys116/frust/main/install.ps1 | iex
```

**Anywhere with Rust**

```sh
cargo binstall frust-rs   # prebuilt binaries
cargo install frust-rs    # from source
```

Binaries are checksummed and installed to `~/.local/bin` (`%LOCALAPPDATA%\frust\bin` on Windows). Run `frust doctor`
afterwards to check your setup. The crates.io package is `frust-rs` because the name `frust` belongs to an unrelated crate.

Windows support is new. The test suite runs on Windows in CI, but it has had far less real-world use than macOS and Linux.

<p align="center"><img src="https://raw.githubusercontent.com/Seggys116/frust/main/.github/assets/divider.svg" alt="" width="880"></p>

## Use

Use `fcargo` wherever you would use `cargo`:

```sh
fcargo build --release
fcargo +nightly test -p mycrate -- --nocapture
fclippy --all-targets
```

| Command | Purpose |
|---|---|
| `frust stats` | Hit rate, time saved, cache and target sizes |
| `frust doctor` | Check the environment frust relies on |
| `frust locks` | Show running and queued builds |
| `frust compact` / `frust gc` | Shrink target directories and prune the cache |
| `frust env` | Print shell exports so IDEs and rust-analyzer share the cache |
| `frust where` | Print the central target directory for this workspace |

Anything frust does not recognise is passed straight to cargo. Set `FRUST_BYPASS=1` to run cargo completely untouched.

On macOS, `FRUST_LINK=smart fcargo build` opts eligible dev binaries into the direct native-linker path. Measure actual edit builds with `frust bench --linking --runs 7`; see [configuration](docs/configuration.md) for eligibility and controls.

## Builds that overlap

<p align="center"><img src="https://raw.githubusercontent.com/Seggys116/frust/main/.github/assets/locks.svg" alt="Builds in different profiles run at once, builds in the same profile run in order, and identical builds share one build" width="880"></p>

Builds in different profiles run in parallel. Builds in the same profile queue in order, and an identical build joins the
one already running instead of waiting. A `--target` build also takes its profile's host slot, so it queues behind other
builds of that profile.

<p align="center"><img src="https://raw.githubusercontent.com/Seggys116/frust/main/.github/assets/divider.svg" alt="" width="880"></p>

## Results

<p align="center"><img src="https://raw.githubusercontent.com/Seggys116/frust/main/.github/assets/results.svg" alt="Measured debug build times: cold, warm restore, no-op and real warm-session edit; warm restore 28.9x faster" width="880"></p>

Measured on Intel(R) Core(TM) Ultra 7 255H, ext4, rustc 1.97.1: median of 7 runs. A warm debug clean rebuild was 28.9x faster. Combined target and cache storage after 200 edits was 35.6% smaller than Cargo, with compaction assessed after every build. Initial two-project combined allocation was 221.1 MiB for Cargo versus 260.2 MiB for frust. The figures include cold-build, no-op and edit overhead; paired release binaries and runtime outputs matched. See the [method and complete results](docs/benchmarks.md).

<p align="center"><img src="https://raw.githubusercontent.com/Seggys116/frust/main/.github/assets/release-profiles.svg" alt="Warm-cache release rebuilds with default release, ThinLTO and full/fat LTO settings" width="880"></p>

<p align="center"><img src="https://raw.githubusercontent.com/Seggys116/frust/main/.github/assets/growth.svg" alt="Combined target and cache allocation after 200 edits, including automatic and disabled cache compression" width="880"></p>

<p align="center"><img src="https://raw.githubusercontent.com/Seggys116/frust/main/.github/assets/divider.svg" alt="" width="880"></p>

## Updating

After a successful build on an interactive terminal, frust prints a one-line notice when a newer release exists. Run
`frust self-update` to install it; the command verifies the download's SHA-256 before replacing anything. It is not
`frust update`, so `cargo update` keeps working. Set `FRUST_UPDATE_CHECK=off` (or run `frust self-update --disable`)
to stop the daily check.

<p align="center"><img src="https://raw.githubusercontent.com/Seggys116/frust/main/.github/assets/divider.svg" alt="" width="880"></p>

## Learn more

[How it works](https://github.com/Seggys116/frust/blob/main/docs/how-it-works.md) &middot; [Configuration](https://github.com/Seggys116/frust/blob/main/docs/configuration.md) &middot; [Benchmarks](https://github.com/Seggys116/frust/blob/main/docs/benchmarks.md) &middot; [Contributing](https://github.com/Seggys116/frust/blob/main/CONTRIBUTING.md) &middot; [Security](https://github.com/Seggys116/frust/blob/main/SECURITY.md) &middot; [Changelog](https://github.com/Seggys116/frust/blob/main/CHANGELOG.md)

<br>

<p align="center"><sub>MIT licensed &middot; &copy; 2026 Zak Noble-Clarke</sub></p>

<div align="center">

<a href="https://www.buymeacoffee.com/seggy116"><img src="https://cdn.buymeacoffee.com/buttons/v2/default-violet.png" alt="Buy me a coffee" height="50" /></a>

</div>
