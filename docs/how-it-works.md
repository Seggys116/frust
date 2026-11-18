# How fRust is designed to work

fRust will build and distribute a toolchain from a pinned upstream Rust source line. The compiler, standard library, Cargo and rustdoc will be built and versioned together. The toolchain is intended to support the same commands, editions, crates, build scripts and procedural macros as the matched upstream release on supported hosts. Use a matched toolchain for Rust compiler-specific artifacts such as crate metadata and compiled procedural macros. [Rust compiler metadata](https://rustc-dev-guide.rust-lang.org/backend/libs-and-metadata.html)

## Incremental builds

`fcargo` resolves dependencies, prepares build scripts and schedules compilation. Within `frustc`, a source edit can invalidate compiler queries through recorded dependencies. Unaffected analysis and code generation can be reused when their inputs and outputs are still valid. A one-file change can still affect many items through types, generics, macros, constants or inlining. [Rust compiler incremental compilation](https://rustc-dev-guide.rust-lang.org/queries/incremental-compilation-in-detail.html)

## Release code generation and linking

The requested optimization profile governs code generation. With full LTO enabled, the compiler can reuse valid work from before the whole-program optimization step. A changed whole-program input can still require that step to run again. Linux and macOS native linking will be measured independently, with platform behavior and explicit linker selection preserved. [Cargo profiles](https://doc.rust-lang.org/cargo/reference/profiles.html) · [LLVM LTO](https://llvm.org/docs/LinkTimeOptimization.html)
