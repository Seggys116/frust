# Configuration design

The patched Cargo defaults `fcargo` build outputs to per-user storage. Workspaces and toolchains get separate mutable output directories. Cross-workspace compiler artifact reuse remains experimental.

The user-level JSON file accepts separate paths for the target root and cache root. Either path can point to temporary storage. Cleanup on reboot depends on how the operating system manages the chosen directory. The default paths use each platform’s user cache location. The installer and toolchain files live separately from managed build data.

Explicit Cargo directory controls take precedence. Tooling should discover final artifact paths through Cargo metadata or build messages. [Cargo build directories](https://doc.rust-lang.org/cargo/reference/build-cache.html) · [Cargo configuration](https://doc.rust-lang.org/cargo/reference/config.html)
