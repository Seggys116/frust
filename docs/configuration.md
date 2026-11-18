# Configuration design

The rewrite plans to keep build outputs and compiler cache data in centralized, per-user storage. Independent workspaces and toolchains get separate mutable output directories under that root. The compiler can share artifacts across workspaces when their full input identity permits reuse.

A persistent user-level JSON file is planned to allow separate paths for the target root and compiler cache. Either path can point to temporary storage. Cleanup on reboot depends on how the operating system manages the chosen directory. The default paths will use each platform’s user cache location. The installer and toolchain files will live separately from managed build data.

Explicit Cargo directory controls will continue to work. Tooling should discover final artifact paths through Cargo metadata or build messages. [Cargo build directories](https://doc.rust-lang.org/cargo/reference/build-cache.html) · [Cargo configuration](https://doc.rust-lang.org/cargo/reference/config.html)
