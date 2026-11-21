# Configuration

fRust keeps build outputs in per-user central storage when Cargo is started through `fcargo`. The behavior below is implemented in the patched Cargo (`support/patches/0001-frust-central-target.patch`) and `fcargo`, and the Linux paths are covered by tests in this repository. The macOS paths are computed by the same code but have not been run on macOS.

## Central target directory

The patched Cargo uses a central target directory only when all of these hold:

- Cargo was started by `fcargo`, `frust` or `fclippy` (which set `FRUST_INVOCATION=fcargo`) or its executable is named `fcargo`, `frust` or `fclippy`.
- `FRUST_CENTRAL_TARGET` is not `off`.
- No target directory or build directory was chosen another way, for example `CARGO_TARGET_DIR` or a Cargo configuration setting.
- The workspace is not an ephemeral or embedded manifest.

Plain `cargo` keeps the stock `target/` default. When the central default applies, Cargo fails with a message if `HOME` is not an absolute path, if `config.json` is invalid, or if the toolchain id is rejected.

The directory is `<target_root>/targets/<xx>/<rest>`, where `xx<rest>` is the hex sha-256 of the toolchain id, a NUL byte and the canonical workspace path. Different workspaces and different toolchains therefore get separate directories, and a symlink to a workspace maps to the same directory as the workspace. The toolchain id is, in order, `FRUST_TOOLCHAIN_ID`, the trimmed contents of the `frust-toolchain-id` file in the toolchain root (the parent of `bin/`), or `frust-unidentified`. An id must be non-empty and must not contain `/`, `\` or a NUL byte.

## User config file

`config.json` is optional. A missing file means defaults. It is a JSON object with exactly these fields, and an unknown field is an error:

```
{"version": 1, "target_root": "/absolute/path", "cache_root": "/absolute/path"}
```

`version` is required and must be the integer 1. `target_root` and `cache_root` are optional and must be absolute paths. Either can point to temporary storage. Cleanup on reboot depends on how the operating system manages the chosen directory.

| | Linux | macOS |
| --- | --- | --- |
| Config file | `$XDG_CONFIG_HOME/frust/config.json`, else `~/.config/frust/config.json` | `~/Library/Application Support/fRust/config.json` |
| Default roots | `$XDG_CACHE_HOME/frust`, else `~/.cache/frust` | `~/Library/Caches/fRust` |

An `XDG_*` value that is empty or not absolute is ignored. Targets go under `<target_root>/targets`. The target root and the cache root default to the same directory.

## Artifact cache directory

### FRUST_HOME

`FRUST_HOME=/absolute/path` moves both roots for that invocation: targets go under `$FRUST_HOME/targets` and the artifact cache under `$FRUST_HOME/artifacts`, exactly as if `target_root` and `cache_root` were both `$FRUST_HOME`. It outranks the roots in `config.json` (the other settings of that file still apply) and nothing is written to the default locations. An empty value is ignored and a relative path is rejected with a diagnostic naming the variable.

The patched Cargo creates `<cache_root>/artifacts` and a `MARKER` file containing `frust-cache-v1`. If the path is not a directory, cannot be written, or holds an unreadable or different marker, the check reports a fallback. The patched Cargo ignores that result for central targets. The unit artifact cache below stores in `units/v1` inside this directory and stays off when the check does not report the directory ready. The opt-in compiler object cache described in [how-it-works](how-it-works.md) is stored next to the compiler's output object, not here.

## Unit artifact cache

The patched Cargo (`support/patches/0007-frust-unit-cache.patch`) keeps a content-addressed store of finished rustc outputs for registry, git and directory-source dependencies, and of heavy build script runs of registry and git packages. It lives in `<cache_root>/artifacts/units/v1` (default `~/.cache/frust/artifacts/units/v1` on Linux). `HOME` must be an absolute path. [How it works](how-it-works.md#unit-artifact-cache) describes what is stored, the key and the fallbacks.

The cache is on when Cargo was started by `fcargo`, `frust` or `fclippy` (`FRUST_INVOCATION=fcargo`) or its executable has one of those names. It is available on Linux and macOS only.

| Variable | Effect |
| --- | --- |
| `FRUST_UNIT_CACHE` | `1` or `on` turns the cache on for any invocation. `0` or `off` turns it off. Unset or any other value: on under `fcargo`, off otherwise. |
| `FRUST_UNIT_CACHE_MAX_BYTES` | Size budget of the store in bytes. A missing, zero or unparsable value means 8 GiB. |
| `FRUST_UNIT_CACHE_SCRIPT_MIN_MS` | Shortest build script run, in milliseconds, that is stored. Default 3000. `0` stores every eligible run. An unparsable value means the default. |
| `FRUST_UNIT_CACHE_DEBUG` | `1` prints one line per decision to stderr, prefixed `frust-unit-cache:`. Units log `hit`, `miss` with a reason, `store` and `skip` with a reason. Build scripts log `script-hit`, `script-miss`, `script-store` and `script-skip`. |

After a store, a size scan runs at most once per process, and only when the size counter is missing, older than a day or over the budget. If the scan finds the total over the budget, entries are evicted least recently used first until the store is at 80 percent of it. The same scan removes temporary files and incomplete entries more than an hour old. The `FRUST_UNIT_CACHE*` variables are not part of the build script key.

## Direct entry

In the packaged toolchain `fcargo`, `frust` and `fclippy` are hard links of the patched `cargo` (`support/patches/0008-frust-direct-entry.patch`), and the terminal progress launcher is installed as `fcargo-ui`. The patched Cargo looks at the file name it was started as:

| Name | Behavior |
| --- | --- |
| `fcargo` | Cargo with the fRust defaults described below. |
| `frust` | Identical to `fcargo`, plus the `where` command below. |
| `fclippy` | `fcargo clippy ARGS...`. The `clippy` subcommand is inserted for you, after a leading `+toolchain` if there is one: `fclippy +nightly --all-targets` is `fcargo +nightly clippy --all-targets`. |

On Linux and macOS these names run Cargo directly, with `FRUST_INVOCATION=fcargo`, `CARGO`, the toolchain's `bin` directory put first on `PATH` (so its `cargo-clippy`, `cargo-fmt` and `rustfmt` win over a rustup proxy of the same name) and, when `RUSTC` is unset and the compiler exists, `RUSTC` set. `RUSTC` is the sibling `frustc`, except for the `clippy` subcommand, where it is the sibling `rustc` (the same binary): clippy-driver runs as a wrapper and only recognises the compiler path Cargo hands it when the file stem is `rustc`. Clippy runs therefore skip the behaviors keyed on the name `frustc` (content fingerprints, the fat LTO defaults and the interface cutoff). The central target directory and the unit cache apply to all three names. A run is handed to `fcargo-ui` with the same arguments (with `clippy` inserted for `fclippy`) when one of these holds:

- Progress is not plain. It is plain when `TERM=dumb`, when `FRUST_PROGRESS` is `plain` or `off`, or when `FRUST_PROGRESS` is not `static` or `interactive` and stderr is not a terminal.
- `FRUST_CARGO` is set and not empty.
- The first argument starts with `+`.

A plain `fclippy` cannot change its own argument list, so it starts the same executable again as `cargo` with `clippy` inserted; the environment above is inherited and the second start does not enter this logic again.

If `FRUST_CARGO` names an `fcargo`, `frust` or `fclippy` that is the same file as this executable, it exits with an error naming the problem. On other hosts these names are still the patched Cargo, but the direct entry code is not compiled.

### `frust where`

`frust where` (and `fcargo where`) prints, on one line, the target directory a build in the workspace of the current directory would use, and exits 0. It is the central directory described above, or `<workspace>/target` with `FRUST_CENTRAL_TARGET=off`, or whatever `CARGO_TARGET_DIR` or `build.target-dir` chooses, honoring `FRUST_HOME` and `config.json` like a build. `frust where --manifest-path PATH` asks about another workspace. Outside a workspace it fails with Cargo's usual `could not find Cargo.toml` error (exit 101).

The command is recognised only under the names `fcargo` and `frust`, and only as exactly `where` or `where --manifest-path PATH`; any longer command line goes to Cargo unchanged. It is handled before a terminal hand-over, so it never starts the progress launcher. Stock Cargo has no `where` command, but a `cargo-where` plugin or a `where` alias would be shadowed by `fcargo where` and `frust where`; run `cargo where` (the plain `cargo` entry) for those.

## Installer variants

On Linux x86-64 a release has two toolchain archives. `frust-toolchain-x86_64-unknown-linux-gnu` is the baseline build for any x86-64 CPU. `frust-toolchain-x86_64-unknown-linux-gnu-v3` has its compiler and tools built for x86-64-v3; the standard library and the programs you compile keep the baseline target. Each has its own `.sha256` and `.id` file, and the v3 toolchain id ends in `-v3`, so both can sit in `versions/` at once. Other hosts (macOS, Linux on ARM) have one archive and no variants.

`frust-install` reads `FRUST_VARIANT` when it downloads:

- `auto` (the default, also an empty value): the v3 build when the CPU reports every extension it uses and the release lists a v3 archive, otherwise the baseline. The extensions are AVX, AVX2, BMI1, BMI2, FMA, MOVBE, LZCNT, F16C and XSAVE, on top of the x86-64-v2 set (CMPXCHG16B, POPCNT, SSE3, SSSE3, SSE4.1, SSE4.2). A virtual machine that hides any of them gets the baseline.
- `baseline` or `v3`: that build. `v3` fails when the release has no v3 archive, and on a host that has no variants. On a CPU that lacks an extension it installs with a warning, because the toolchain may not run there. Any other value is a usage error (exit status 2).

`state.json` records `current_variant` and `previous_variant` next to `current` and `previous`, read from the optional `variant` member of each archive's `manifest.json`. `rollback` switches both back. `install` and `update` keep the installed variant unless `FRUST_VARIANT` is `baseline` or `v3`, so a machine moved to another CPU is not changed behind your back, and to move an installed baseline to v3 you run `FRUST_VARIANT=v3 frust-install update`. An installation made before variants existed records none, so its first update chooses as `auto` does. `FRUST_VARIANT` is ignored with `--archive`, which installs the variant its manifest names.

## Targets and components

An archive already has the standard library for the targets listed in the [README](../README.md#use). `frust-install` (also installed as `frustup`, which behaves the same and prints its help when run with no command) adds and removes others, and adds the compiler-independent components of the official Rust 1.100.0 channel:

```sh
frust-install target list [--installed]
frust-install target add thumbv7em-none-eabihf
frust-install target remove thumbv7em-none-eabihf
frust-install component list [--installed]
frust-install component add rust-src
frust-install component remove rust-src
frust-install show
frust-install which cargo-clippy
```

`target add` takes any target of the channel. `component add` takes `rust-docs`, `rust-src` or `llvm-tools`, and leaves one that the toolchain already has alone (the archive bundles `rust-docs` and `rust-src`). Cargo, rustc, rustdoc, clippy, rustfmt and rust-analyzer (with its proc-macro server) are built into every archive and are replaced by an update, so they are not added or removed; the official rust-analyzer links the official compiler driver and cannot start next to the fRust compiler. `show` prints the installed version, variant, host, targets and components, and `which TOOL` prints the path of a tool of the installed toolchain. All of them take `--prefix`.

The commands read the channel manifest of the Rust release the toolchain was built from (`https://static.rust-lang.org/dist/channel-rust-1.100.0.toml`), check it against its published sha-256, download the package for the host and target, check the package against the sha-256 in the manifest, and validate the archive before unpacking it into the installed toolchain. A package never overwrites a file that is already there, so the standard library of the host stays the one fRust built. This is sound for the same reason the bundled libraries are: the fRust compiler reports the same `rustc -vV` as official Rust 1.100.0 ([how it works](how-it-works.md#what-the-toolchain-contains)). `FRUST_DIST_BASE` replaces the download location, for a mirror.

The installer records every target and component you add in `extras.list` in the prefix. After `frust-install install`, `update` or `upgrade` downloads a new toolchain, it adds the recorded ones to it again, except those the new archive already bundles. A failure in that step is reported with the command that retries it and does not undo the update. `rollback` only switches back to the previous version tree.

## Environment variables

| Variable | Read by | Effect |
| --- | --- | --- |
| `FRUST_PROGRESS` | `fcargo` | `plain` or `off`: hand over to Cargo with no progress output. `static`: print one final frame. `interactive`: repaint on a terminal, otherwise static. Unset or any other value: interactive on a terminal, plain otherwise. `TERM=dumb` forces plain. |
| `FRUST_PROGRESS_COLOR` | `fcargo` | `always` or `never`. Otherwise color is on for a terminal whose `TERM` is not `dumb`. |
| `FRUST_BUILD_REPORT` | `fcargo` | When set to any value, print a build report with rows for analysis, code generation, full LTO and native link. Each row currently reads `Unavailable` because `fcargo` does not observe those phases. Not printed in plain mode. |
| `FRUST_CARGO` | `fcargo` | Path of the Cargo executable to run. Default: a `cargo` next to `fcargo`, else `cargo` on `PATH`. |
| `FRUST_INVOCATION` | patched Cargo | Set to `fcargo` by `fcargo` for the Cargo it starts. |
| `FRUST_CENTRAL_TARGET` | patched Cargo | `off` disables the central target default. |
| `FRUST_TOOLCHAIN_ID` | patched Cargo | Overrides the toolchain id used in the central target path. |
| `FRUST_EXPERIMENTAL_CODEGEN_CACHE` | `frustc` | `1` enables the post-LTO object cache. |
| `FRUST_EXPERIMENTAL_RELR` | `frustc` | `1` enables packed relative relocations on eligible GNU links. |
| `FRUST_DIST_BASE` | `frust-install`, `frustup` | Download location of the official channel for `target` and `component` commands. Default `https://static.rust-lang.org/dist`. |
| `FRUST_VARIANT` | `frust-install` | `auto`, `baseline` or `v3`: which Linux x86-64 build to install. Default `auto`. See [Installer variants](#installer-variants). |

`fcargo` also sets `RUSTC` to a `frustc` next to the Cargo executable when `RUSTC` is unset and that file exists. It forwards Cargo's exit code. Plain mode, `--quiet`, `-q`, `--message-format` and `--json` pass straight through to Cargo.

Explicit Cargo directory controls take precedence over the central default. Tooling should discover final artifact paths through Cargo metadata or build messages. [Cargo build directories](https://doc.rust-lang.org/cargo/reference/build-cache.html) · [Cargo configuration](https://doc.rust-lang.org/cargo/reference/config.html)

## Cooperative checks

On x86-64 GNU/Linux, eligible `fcargo check` invocations share check units by default when they use the same workspace and build directory. Cargo prepares the graph without waiting for another process's unit locks. Once a unit's dependencies are ready, the scheduler checks the fingerprint and tracked inputs under the unit lock before reusing the outputs and replaying diagnostics. Set `FRUST_UNIT_JOIN=off` to use ordinary Cargo scheduling.

Joining uses the current Cargo build-directory layout, local storage, and the invocation's own jobserver. Graphs with build scripts, proc macros or artifact dependencies use ordinary Cargo scheduling, as do checks with `--keep-going`, `--timings`, or Cargo's explicit fine-grained locking flag. Contended units defer before taking a job slot, so independent work can proceed even with `-j1`. If a check stalls while retaining reader locks, Cargo releases them and retries serially. That retry can repeat unit progress messages. Switching between modes can cause a rebuild because their fingerprint identities differ.

Run a full `cargo clean` after active invocations finish, as Cargo can remove the whole build directory without taking its locks. The native qualification covers Linux x86-64. The legacy `FRUST_EXPERIMENTAL_UNIT_JOIN=1` opt-in remains available on macOS for qualification. [Concurrent check probe](../qualification/cooperative-builds.py)

In the 19 November 2026 Linux check, all five regex metadata artifacts matched the ordinary Cargo path byte for byte. Twelve alternating warm checks per mode reported every artifact fresh. The syn workspace exercised ordinary scheduling for a graph with build-time dependencies and produced identical metadata with either setting.

## Default parallel jobs

When neither `-j`/`--jobs` nor `build.jobs` (including `CARGO_BUILD_JOBS`) is set, the patched Cargo (`support/patches/0009-frust-memory-aware-jobs.patch`) starts one job per logical CPU, as Cargo does. If the memory available to the process holds fewer than that many jobs at 1 GiB each, it starts one job per whole GiB available instead, and never fewer than one. Available memory is the smaller of the host's `MemAvailable` and the headroom under the lowest `memory.max` or `memory.high` in the process's cgroup v2 ancestry (current use less inactive file cache). A host with plenty of memory keeps the CPU count.

An explicit count (positive, negative or `default`), an external jobserver in the environment, and any platform other than Linux keep Cargo's behavior. cgroup v1 limits are not read. `-v` prints a note when the default is lowered. Codegen, LTO and profile settings are not touched.

The 1 GiB figure is about twice the largest single-process peak resident set recorded in `qualification/BASELINES.md` (537024 kB, release builds at fat LTO of four crate graphs on one Linux x86-64 host). Those figures are per process, not concurrent, and no build under a memory limit is recorded yet. `qualification/memory-jobs-bench.py` records CPU, wall time, `ru_maxrss` from `wait4`, the scope's `memory.peak` and OOM kills for one build under a limit.

## Native fat-LTO defaults

When `fcargo` selects `frustc` for a native x86-64 GNU Linux build, local packages in release-derived profiles using fat LTO at opt level 3 default to incremental compilation and 16 codegen units. Use the project's ordinary release or custom profile command. Cargo honors explicit profile, package, environment, and command-line settings as well as its existing CI incremental policy.

The incremental default applies when both fields are unspecified. If incremental compilation is already enabled, fCargo supplies 16 codegen units only when the profile leaves that count unset. Build scripts, proc macros, registry dependencies, and cross-target units keep their usual defaults.

In the 2026-11-19 Linux samples, 16 codegen units reduced edited build time by 23.4% for ripgrep and 4.7% for fd. Ripgrep's stripped executable grew by 3.2%. Both builds still ran full LTO. [Measurements](../qualification/SEMANTIC-LTO.md)
