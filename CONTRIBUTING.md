# Contributing

This branch is preparing the fRust compiler rewrite. The [README](README.md) describes its status and links to the design documentation. Repository working conventions are in [AGENTS.md](AGENTS.md).

## Changes and verification

- Keep each commit scoped and describe its purpose. Update the user-facing docs when a command, configuration option or supported platform changes.
- Record the exact upstream compiler, Cargo, LLVM, linker, SDK, host and target whenever reporting compatibility or performance results.
- For compiler changes, show which upstream and project tests ran. For performance changes, include the workload, build state, options and before-and-after measurements. See the [benchmark methodology](docs/benchmarks.md).
- Keep architecture plans, implementation briefs, working research and private benchmark material local. Use public fixtures for shareable reproductions.

## Continuous integration

`.github/workflows/ci.yml` runs on every pull request and push to `main`. Its jobs are independent and run in parallel. The final `ci` job is the single required check.

It includes a job that compares every pin in `support/official-components.lock` with the official channel manifest, so a mistyped or stale hash fails there and not in the middle of a release build. The shell job also checks the scripts that run on the macOS runners for bash 3.2 syntax.

Two workflows are manual and run only from the Actions tab or `gh workflow run`:

- `Toolchains` builds the unpatched control and the patched fRust stage-2 toolchains from the pinned sources and checks that the fRust package launches, including clippy and rustfmt, and passes the parity proof. Built toolchains are cached by recipe and patch hash.
- `Benchmark` builds both toolchains, then runs the paired build samples, runtime and stripped-size measurements and the release gates from the [qualification protocol](qualification/PROTOCOL.md), one runner per series. Use few pairs with `enforce` off for a smoke run. A full run needs 30 pairs for the median series and 60 for leaf and wide edits, and takes several hours.

GitHub's shared runners are noisy and their hardware is not recorded per job, so a Benchmark result checks the pipeline. A performance claim still needs the dated host evidence the [benchmark methodology](docs/benchmarks.md) requires.

## What a release archive contains

Every archive is a complete toolchain: the patched Cargo (`cargo`, `fcargo`, `frust`), `rustc` (`frustc`), `rustdoc` (`frustdoc`), `cargo-clippy` and `clippy-driver` (`fclippy`), `rustfmt` and `cargo-fmt` (`frustfmt`), `rust-analyzer`, the `rust-docs` HTML documentation, the installer as `frustup`, the host standard library and the standard libraries of the extra targets of its platform. Clippy, rustfmt, rust-analyzer and its proc-macro server are built from the pinned sources. The extra targets, rust-docs and rust-src are the official packages pinned by sha-256 in `support/official-components.lock` and are valid because the compiler reports the same `rustc -vV` as the official release; see [how it works](docs/how-it-works.md#what-the-toolchain-contains). When you change what is bundled, update the lock (see [build assets](docs/build-assets.md#official-components)), the validation suite and the docs together.

## Validating a toolchain

`support/toolchain-suite.sh` runs an installed toolchain the way a user does, with no ambient state (throwaway `HOME`, `CARGO_HOME` and `FRUST_HOME`, and a `PATH` of the toolchain plus `/usr/bin:/bin`, so a rustup on your machine cannot make a broken toolchain pass). It runs every command, builds, tests, lints, formats and documents projects, runs rust-analyzer, builds for the bundled targets, and exercises the installer's `target` and `component` commands. It prints PASS, FAIL or SKIP for each check, names the reason for every skip, and exits 1 when anything failed.

```sh
frust-install --archive frust-toolchain-<target>.tar.gz --checksum frust-toolchain-<target>.tar.gz.sha256 --prefix /tmp/frust-prefix
support/toolchain-suite.sh /tmp/frust-prefix /tmp/frust-suite
```

The first argument is an installation prefix (it has `current/bin`) or a toolchain tree (it has `bin`). The second is an empty or missing work directory. A third argument (`linux`, `macos-aarch64` or `macos-x86_64`) selects the bundle whose tools and targets are expected and defaults to the host's. `FRUST_SUITE_ONLINE=1` also runs `target add` against the real network, and `FRUST_SUITE_TIMEOUT` sets the seconds allowed per command (default 900). The release workflow runs the suite against every archive after installing it with `frust-install`.

## Releasing

A release is a `v*` tag on a commit that is on `main` and has a passing CI run. Before tagging, set the version in `crates/Cargo.toml` (both `workspace.package.version` and the `=` pin in `workspace.dependencies`), update `crates/Cargo.lock`, move the changelog entries out of Unreleased, and merge to `main`. Pushing the tag then runs `.github/workflows/release.yml`:

1. It checks the tag against the crate version and the CI result for the commit, and checks `support/official-components.lock`.
2. It builds the fRust toolchain natively for Linux x86-64 (baseline and x86-64-v3), macOS Apple Silicon and macOS Intel (cached by recipe and patch hash), launches each, and runs the parity proof on Linux. Each build includes clippy and rustfmt.
3. On each platform it packages `frust-toolchain-<target>.tar.gz` with the official components, its `.sha256` and its `.id` file, installs it with `frust-install` into a scratch prefix and runs `support/toolchain-suite.sh` on the installed copy.
4. It publishes the GitHub release, which is what `frust-install` and `frust-install update` read, only when every archive passed step 3, and then publishes `frust-rs-core` and `frust-rs` to crates.io. The crates job needs a `CARGO_REGISTRY_TOKEN` repository secret.

Prove a commit before tagging it with a dry run, which does steps 1 to 3 exactly as a tag does and publishes nothing, whatever ref it runs on:

```sh
gh workflow run release.yml --ref main
gh run watch
```

Add `-f rebuild=true` to ignore the cached toolchains and build everything again (about two hours with the macOS Intel build; a run that reuses the cache takes about 25 minutes). Only a green dry run on the commit to be tagged shows that the tag will publish. Publishing is irreversible, so tag only with explicit authorization.

Follow the [code of conduct](CODE_OF_CONDUCT.md) and use the private reporting channel for [security issues](SECURITY.md).
