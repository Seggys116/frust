# Security policy

This policy covers the fRust compiler toolchain on this branch. The earlier build accelerator, its `FRUST_HOME` directory and its install scripts are no longer in the active branch.

## Reporting a vulnerability

Please report vulnerabilities privately through GitHub's
[private vulnerability reporting](https://github.com/Seggys116/frust/security/advisories/new)
instead of a public issue. Include the version, platform, and a reproduction if you have one.

You can expect an acknowledgement within a few days. Fixes ship in a new release, and the advisory credits the
reporter unless they prefer otherwise.

## Scope

Of particular interest, by component:

- **`frust-install`.** It verifies the archive's sha-256 against a checksum file you supply, then extracts with the system `tar` into a staging directory under the prefix. It requires `bin/rustc`, `bin/cargo`, `bin/rustdoc` and a `manifest.json` whose `id` is a single path segment, and swaps the `current` symlink only afterward. It does not download anything, and the checksum file is not authenticated. Archive extraction and path handling, a tree becoming active before verification, and rollback state are in scope.
- **Compiler object cache.** With `FRUST_EXPERIMENTAL_CODEGEN_CACHE=1`, `frustc` reuses objects from a `frust-codegen-cache/` directory next to its output, named by a 64-bit hash of the optimized module. It copies a cached file after checking only that the file exists and is not empty. The object's contents are not re-verified against the key. Anyone who can write that directory can supply code that is linked into the next build. Cache poisoning and key collisions are in scope. A miss, an empty or unreadable entry, or a failed copy compiles normally.
- **Central target directory and config.** The patched Cargo reads `config.json` from the user config directory with a hand-written JSON parser, creates the artifact cache directory and its `MARKER` file under `cache_root`, and derives per-workspace target directories from a hash of the toolchain id and canonical workspace path. Parser behavior, path handling including symlinks, `XDG_*_HOME` and `HOME` handling, and toolchain id validation are in scope. Nothing is stored in the artifact cache directory yet.
- **`fcargo`.** It starts Cargo as a child process, or replaces itself with Cargo, and parses Cargo's JSON messages to draw progress. `FRUST_CARGO` names the executable it runs with the caller's privileges, and it sets `RUSTC` to a sibling `frustc` when `RUSTC` is unset.

Anything that lets one build or user read, poison or corrupt another's cache or target directory is of particular interest.

## Supported versions

The CHANGELOG lists no release of the compiler toolchain yet. Only the latest release receives fixes.
