# Configuration design

The patched Cargo defaults `fcargo` build outputs to per-user storage. Workspaces and toolchains get separate mutable output directories. Cross-workspace compiler artifact reuse remains experimental.

The user-level JSON file accepts separate paths for the target root and cache root. Either path can point to temporary storage. Cleanup on reboot depends on how the operating system manages the chosen directory. The default paths use each platform’s user cache location. The installer and toolchain files live separately from managed build data.

Explicit Cargo directory controls take precedence. Tooling should discover final artifact paths through Cargo metadata or build messages. [Cargo build directories](https://doc.rust-lang.org/cargo/reference/build-cache.html) · [Cargo configuration](https://doc.rust-lang.org/cargo/reference/config.html)

## Cooperative checks

On x86-64 GNU/Linux, eligible `fcargo check` invocations share check units by default when they use the same workspace and build directory. Cargo prepares the graph without waiting for another process's unit locks. Once a unit's dependencies are ready, the scheduler checks the fingerprint and tracked inputs under the unit lock before reusing the outputs and replaying diagnostics. Set `FRUST_UNIT_JOIN=off` to use ordinary Cargo scheduling.

Joining uses the current Cargo build-directory layout, local storage, and the invocation's own jobserver. Graphs with build scripts, proc macros or artifact dependencies use ordinary Cargo scheduling, as do checks with `--keep-going`, `--timings`, or Cargo's explicit fine-grained locking flag. Contended units defer before taking a job slot, so independent work can proceed even with `-j1`. If a check stalls while retaining reader locks, Cargo releases them and retries serially. That retry can repeat unit progress messages. Switching between modes can cause a rebuild because their fingerprint identities differ.

Run a full `cargo clean` after active invocations finish, as Cargo can remove the whole build directory without taking its locks. The native qualification covers Linux x86-64. The legacy `FRUST_EXPERIMENTAL_UNIT_JOIN=1` opt-in remains available on macOS for qualification. [Concurrent check probe](../qualification/cooperative-builds.py)

In the 19 November 2026 Linux check, all five regex metadata artifacts matched the ordinary Cargo path byte for byte. Twelve alternating warm checks per mode reported every artifact fresh. The syn workspace exercised ordinary scheduling for a graph with build-time dependencies and produced identical metadata with either setting.

## Native fat-LTO defaults

When `fcargo` selects `frustc` for a native x86-64 GNU Linux build, local packages in release-derived profiles using fat LTO at opt level 3 default to incremental compilation and 16 codegen units. Use the project's ordinary release or custom profile command. Cargo honors explicit profile, package, environment, and command-line settings as well as its existing CI incremental policy.

The incremental default applies when both fields are unspecified. If incremental compilation is already enabled, fCargo supplies 16 codegen units only when the profile leaves that count unset. Build scripts, proc macros, registry dependencies, and cross-target units keep their usual defaults.

In the 2026-11-19 Linux samples, 16 codegen units reduced edited build time by 23.4% for ripgrep and 4.7% for fd. Ripgrep's stripped executable grew by 3.2%. Both builds still ran full LTO. [Measurements](../qualification/SEMANTIC-LTO.md)
