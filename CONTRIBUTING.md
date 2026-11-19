# Contributing

This branch is preparing the fRust compiler rewrite. The [README](README.md) describes its status and links to the design documentation. Repository working conventions are in [AGENTS.md](AGENTS.md).

## Changes and verification

- Keep each commit scoped and describe its purpose. Update the user-facing docs when a command, configuration option or supported platform changes.
- Record the exact upstream compiler, Cargo, LLVM, linker, SDK, host and target whenever reporting compatibility or performance results.
- For compiler changes, show which upstream and project tests ran. For performance changes, include the workload, build state, options and before-and-after measurements. See the [benchmark methodology](docs/benchmarks.md).
- Keep architecture plans, implementation briefs, working research and private benchmark material local. Use public fixtures for shareable reproductions.

## Continuous integration

`.github/workflows/ci.yml` runs on every pull request and push to `main`. Its jobs are independent and run in parallel. The final `ci` job is the single required check.

Two workflows are manual and run only from the Actions tab or `gh workflow run`:

- `Toolchains` builds the unpatched control and the patched fRust stage-2 toolchains from the pinned sources and checks that the fRust package launches and passes the parity proof. Built toolchains are cached by recipe and patch hash.
- `Benchmark` builds both toolchains, then runs the paired build samples, runtime and stripped-size measurements and the release gates from the [qualification protocol](qualification/PROTOCOL.md), one runner per series. Use few pairs with `enforce` off for a smoke run. A full run needs 30 pairs for the median series and 60 for leaf and wide edits, and takes several hours.

GitHub's shared runners are noisy and their hardware is not recorded per job, so a Benchmark result checks the pipeline. A performance claim still needs the dated host evidence the [benchmark methodology](docs/benchmarks.md) requires.

## Releasing

A release is a `v*` tag on a commit that is on `main` and has a passing CI run. Before tagging, set the version in `crates/Cargo.toml` (both `workspace.package.version` and the `=` pin in `workspace.dependencies`), move the changelog entries out of Unreleased, and merge to `main`. Pushing the tag then runs `.github/workflows/release.yml`:

1. It checks the tag against the crate version and the CI result for the commit.
2. It builds the fRust toolchain natively for Linux x86-64, macOS Apple Silicon and macOS Intel (cached by recipe and patch hash), launches each, and runs the parity proof on Linux.
3. On each platform it packages `frust-toolchain-<target>.tar.gz`, its `.sha256` and its `.id` file, installs them with `frust-install` into a scratch prefix and builds a project with the result.
4. It publishes the GitHub release, which is what `frust-install` and `frust-install update` read, and then publishes `frust-rs-core` and `frust-rs` to crates.io. The crates job needs a `CARGO_REGISTRY_TOKEN` repository secret.

Run the workflow by hand from a branch to do steps 1 to 3 without publishing anything. Publishing is irreversible, so tag only with explicit authorization.

Follow the [code of conduct](CODE_OF_CONDUCT.md) and use the private reporting channel for [security issues](SECURITY.md).
