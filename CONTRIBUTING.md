# Contributing

Thanks for helping. frust is a thin layer over the user's own cargo and rustc, so the main rule is: arguments,
output and exit codes pass through unchanged unless a documented feature says otherwise.

## Set up

```sh
git clone https://github.com/Seggys116/frust
cd frust
cargo build
cargo test
```

Rust 1.89 or newer is required (edition 2024).

## Before opening a pull request

```sh
cargo clippy --all-targets
cargo test
```

CI runs the same checks on Linux, macOS and Windows, plus an MSRV build and cross-target checks.

## Guidelines

* A cache hit must be byte-for-byte what rustc would have produced. If correctness is in doubt, do not cache.
* Platform-specific code goes behind `cfg` gates with an equivalent on every supported platform, or a graceful fallback.
* Integration tests use an offline vendored registry and an isolated `FRUST_HOME`. Do not make tests touch the network or the real cache.
* Add a regression test for every bug fix.

## Releases

Maintainers tag `vX.Y.Z`. The release workflow builds archives for Linux, macOS and Windows, publishes them with
checksums, and the install scripts and `cargo binstall` pick them up.
