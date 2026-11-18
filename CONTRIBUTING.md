# Contributing

This branch is preparing the fRust compiler rewrite. The [README](README.md) describes its status and links to the design documentation. Repository working conventions are in [AGENTS.md](AGENTS.md).

## Changes and verification

- Keep each commit scoped and describe its purpose. Update the user-facing docs when a command, configuration option or supported platform changes.
- Record the exact upstream compiler, Cargo, LLVM, linker, SDK, host and target whenever reporting compatibility or performance results.
- For compiler changes, show which upstream and project tests ran. For performance changes, include the workload, build state, options and before-and-after measurements. See the [benchmark methodology](docs/benchmarks.md).
- Keep architecture plans, implementation briefs, working research and private benchmark material local. Use public fixtures for shareable reproductions.

Follow the [code of conduct](CODE_OF_CONDUCT.md) and use the private reporting channel for [security issues](SECURITY.md).
