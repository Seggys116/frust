# Configuration

fRust keeps build outputs in per-user central storage when Cargo is started through `fcargo`. The behavior below is implemented in the patched Cargo (`support/patches/0001-frust-central-target.patch`) and `fcargo`, and the Linux paths are covered by tests in this repository. The macOS paths are computed by the same code but have not been run on macOS.

## Central target directory

The patched Cargo uses a central target directory only when all of these hold:

- Cargo was started by `fcargo` (which sets `FRUST_INVOCATION=fcargo`) or its executable is named `fcargo`.
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

The cache is on when Cargo was started by `fcargo` (`FRUST_INVOCATION=fcargo`) or its executable is named `fcargo`. It is available on Linux and macOS only.

| Variable | Effect |
| --- | --- |
| `FRUST_UNIT_CACHE` | `1` or `on` turns the cache on for any invocation. `0` or `off` turns it off. Unset or any other value: on under `fcargo`, off otherwise. |
| `FRUST_UNIT_CACHE_MAX_BYTES` | Size budget of the store in bytes. A missing, zero or unparsable value means 8 GiB. |
| `FRUST_UNIT_CACHE_SCRIPT_MIN_MS` | Shortest build script run, in milliseconds, that is stored. Default 3000. `0` stores every eligible run. An unparsable value means the default. |
| `FRUST_UNIT_CACHE_DEBUG` | `1` prints one line per decision to stderr, prefixed `frust-unit-cache:`. Units log `hit`, `miss` with a reason, `store` and `skip` with a reason. Build scripts log `script-hit`, `script-miss`, `script-store` and `script-skip`. |

After a store, a size scan runs at most once per process, and only when the size counter is missing, older than a day or over the budget. If the scan finds the total over the budget, entries are evicted least recently used first until the store is at 80 percent of it. The same scan removes temporary files and incomplete entries more than an hour old. The `FRUST_UNIT_CACHE*` variables are not part of the build script key.

## Direct entry

In the packaged toolchain `fcargo` is a hard link of the patched `cargo` (`support/patches/0008-frust-direct-entry.patch`), and the terminal progress launcher is installed as `fcargo-ui`. On Linux and macOS, `fcargo` runs Cargo directly, with `FRUST_INVOCATION=fcargo`, `CARGO` and, when `RUSTC` is unset and a sibling `frustc` exists, `RUSTC` set, unless one of these holds, in which case it executes `fcargo-ui` with the same arguments:

- Progress is not plain. It is plain when `TERM=dumb`, when `FRUST_PROGRESS` is `plain` or `off`, or when `FRUST_PROGRESS` is not `static` or `interactive` and stderr is not a terminal.
- `FRUST_CARGO` is set and not empty.
- The first argument starts with `+`.

If `FRUST_CARGO` names an `fcargo` that is the same file as this executable, it exits with an error naming the problem. On other hosts `fcargo` is still the patched Cargo, but the direct entry code is not compiled.

## Installer variants

On Linux x86-64 a release has two toolchain archives. `frust-toolchain-x86_64-unknown-linux-gnu` is the baseline build for any x86-64 CPU. `frust-toolchain-x86_64-unknown-linux-gnu-v3` has its compiler and tools built for x86-64-v3; the standard library and the programs you compile keep the baseline target. Each has its own `.sha256` and `.id` file, and the v3 toolchain id ends in `-v3`, so both can sit in `versions/` at once. Other hosts (macOS, Linux on ARM) have one archive and no variants.

`frust-install` reads `FRUST_VARIANT` when it downloads:

- `auto` (the default, also an empty value): the v3 build when the CPU reports every extension it uses and the release lists a v3 archive, otherwise the baseline. The extensions are AVX, AVX2, BMI1, BMI2, FMA, MOVBE, LZCNT, F16C and XSAVE, on top of the x86-64-v2 set (CMPXCHG16B, POPCNT, SSE3, SSSE3, SSE4.1, SSE4.2). A virtual machine that hides any of them gets the baseline.
- `baseline` or `v3`: that build. `v3` fails when the release has no v3 archive, and on a host that has no variants. On a CPU that lacks an extension it installs with a warning, because the toolchain may not run there. Any other value is a usage error (exit status 2).

`state.json` records `current_variant` and `previous_variant` next to `current` and `previous`, read from the optional `variant` member of each archive's `manifest.json`. `rollback` switches both back. `install` and `update` keep the installed variant unless `FRUST_VARIANT` is `baseline` or `v3`, so a machine moved to another CPU is not changed behind your back, and to move an installed baseline to v3 you run `FRUST_VARIANT=v3 frust-install update`. An installation made before variants existed records none, so its first update chooses as `auto` does. `FRUST_VARIANT` is ignored with `--archive`, which installs the variant its manifest names.

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
| `FRUST_VARIANT` | `frust-install` | `auto`, `baseline` or `v3`: which Linux x86-64 build to install. Default `auto`. See [Installer variants](#installer-variants). |

`fcargo` also sets `RUSTC` to a `frustc` next to the Cargo executable when `RUSTC` is unset and that file exists. It forwards Cargo's exit code. Plain mode, `--quiet`, `-q`, `--message-format` and `--json` pass straight through to Cargo.

Explicit Cargo directory controls take precedence over the central default. Tooling should discover final artifact paths through Cargo metadata or build messages. [Cargo build directories](https://doc.rust-lang.org/cargo/reference/build-cache.html) · [Cargo configuration](https://doc.rust-lang.org/cargo/reference/config.html)

## Cooperative checks

On x86-64 GNU/Linux, eligible `fcargo check` invocations share check units by default when they use the same workspace and build directory. Cargo prepares the graph without waiting for another process's unit locks. Once a unit's dependencies are ready, the scheduler checks the fingerprint and tracked inputs under the unit lock before reusing the outputs and replaying diagnostics. Set `FRUST_UNIT_JOIN=off` to use ordinary Cargo scheduling.

Joining uses the current Cargo build-directory layout, local storage, and the invocation's own jobserver. Graphs with build scripts, proc macros or artifact dependencies use ordinary Cargo scheduling, as do checks with `--keep-going`, `--timings`, or Cargo's explicit fine-grained locking flag. Contended units defer before taking a job slot, so independent work can proceed even with `-j1`. If a check stalls while retaining reader locks, Cargo releases them and retries serially. That retry can repeat unit progress messages. Switching between modes can cause a rebuild because their fingerprint identities differ.

Run a full `cargo clean` after active invocations finish, as Cargo can remove the whole build directory without taking its locks. The native qualification covers Linux x86-64. The legacy `FRUST_EXPERIMENTAL_UNIT_JOIN=1` opt-in remains available on macOS for qualification. [Concurrent check probe](../qualification/cooperative-builds.py)

In the 19 November 2026 Linux check, all five regex metadata artifacts matched the ordinary Cargo path byte for byte. Twelve alternating warm checks per mode reported every artifact fresh. The syn workspace exercised ordinary scheduling for a graph with build-time dependencies and produced identical metadata with either setting.

## Native fat-LTO defaults

When `fcargo` selects `frustc` for a native x86-64 GNU Linux build, local packages in release-derived profiles using fat LTO at opt level 3 default to incremental compilation and 16 codegen units. Use the project's ordinary release or custom profile command. Cargo honors explicit profile, package, environment, and command-line settings as well as its existing CI incremental policy.

The incremental default applies when both fields are unspecified. If incremental compilation is already enabled, fCargo supplies 16 codegen units only when the profile leaves that count unset. Build scripts, proc macros, registry dependencies, and cross-target units keep their usual defaults.

In the 2026-11-19 Linux samples, 16 codegen units reduced edited build time by 23.4% for ripgrep and 4.7% for fd. Ripgrep's stripped executable grew by 3.2%. Both builds still ran full LTO. [Measurements](../qualification/SEMANTIC-LTO.md)
