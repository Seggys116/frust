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

## Platform code

Everything that differs between Unix and Windows lives in `src/sys/`, which has a `unix.rs` and a `windows.rs` backend
behind `src/sys/mod.rs`. CI fails the lint job if platform calls appear elsewhere.

* No `libc::`, `std::os::unix` or `std::os::windows` outside `src/sys/`. The one exception is the macOS-only `apfs`
  module in `src/compact.rs`.
* Take file identity (device, inode, link count, times) from `sys::stat`, `sys::lstat` or `sys::fstat`. Never build a
  `Meta` with `Meta::from(std::fs::Metadata)`: it works only on Unix, because Windows cannot read file identity from a
  bare `std::fs::Metadata`.
* Turn paths and `OsStr` values into bytes with `OsStr::as_encoded_bytes`, not `OsStrExt`.
* Tests that need Unix-only behaviour get `#[cfg(unix)]`.
* A new platform primitive goes in `src/sys/mod.rs` with an implementation in both backends.

## Releases

Maintainers tag `vX.Y.Z`. The release workflow builds archives for Linux, macOS and Windows, publishes them with
checksums, and the install scripts and `cargo binstall` pick them up.
