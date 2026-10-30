# How frust works

Details behind the README: cache correctness, disk usage, locking and the build display.

## How it stays correct

The cache uses a two-step key, the same idea as ccache's direct mode.

1. A *pre-key* is derived from the toolchain, the normalised command line, `CARGO_*` environment, the
   working directory, and the **contents** of every `--extern` library and native library directory.
2. Earlier real compiles record, per pre-key, the exact source files they read (from rustc's own dep-info,
   with content hashes) and every environment variable the compile observed (`env!`, `option_env!`,
   tracked env in proc macros). A candidate matches only if every file hash and every observed variable
   still holds.

So an edited dependency, a changed `option_env!` value, a different feature set, profile, flag or compiler
all miss. Nothing is cached when it cannot be done safely:

* incremental compiles keep rustc's own incremental behaviour: on a cache miss the original incremental compile runs unchanged, and its result is then stored (at most 4 entries per unit; a hit leaves no incremental session, so the first edit after a restore compiles that crate and its dependents in full; `FRUST_CACHE_PRIMARY_INCREMENTAL=off` disables caching of the packages you are building),
* anything with `--print`, unknown `--emit` kinds, stdin input, response files or non-UTF-8 arguments,
* tool chains such as `clippy-driver rustc ...`,
* (workspace packages you are building are cached too when non-incremental, i.e. release builds; `FRUST_CACHE_PRIMARY=off` disables this.)
* (Incrementally compiled units in unoptimised dev builds are cached, keyed without the session directory and without `CARGO_PRIMARY_PACKAGE`, so `-p` and whole-workspace builds share entries, and kept apart from non-incremental entries. Their outputs are functionally equivalent but not byte-identical to a non-incremental build. Release builds are non-incremental, so release byte-identity is unaffected. `FRUST_CACHE_INCREMENTAL=off` disables this.)

Because rustc flags are hashed as opaque text, new flags in future compilers need no support.

If a `RUSTC_WRAPPER` is already set (for example `sccache`), frust leaves it in place and the compile cache
stays inactive. `frust doctor` says so.

## Smaller build directories

* Opt-in `FRUST_TUNE=on` builds dependencies without debug info in dev profiles (about a tenth smaller, no
  compile-time cost). `line-tables-only` was measured to be both slower and larger, so it is not used. Skipped
  when your project or cargo config defines any profile.
* Reused artifacts are copy-on-write clones on APFS, btrfs and XFS.
* Automatic compaction (after successful builds, `FRUST_AUTO_COMPACT`) removes artifacts no current fingerprint references, keeps the newest incremental session of every crate (so edit builds stay incremental) and keeps the other profile warm for `FRUST_COMPACT_KEEP_DAYS`. On macOS it also compresses idle files with APFS transparent compression (verified byte-for-byte on write) and deduplicates identical files as clones. Cargo sees every unit as fresh afterwards, and anything removed that the compile cache holds comes back as a restore.
* `frust gc` removes superseded artifact variants, stale incremental sessions, idle target directories and
  entries over the cache budget (`FRUST_CACHE_MAX_GIB`, default 16).

## Sharing a repo between builds

* An identical build (same cargo, toolchain, directory, arguments and output-affecting environment) that starts
  while another is running does not queue. It follows the running build's event log, shows live progress in its
  own terminal and finishes within tens of milliseconds of it, with the same result and exit code. If the holder
  dies, the follower takes over and finishes the build.
* Builds in different profiles (debug and release) run in parallel. Cargo locks each profile directory separately, and so
  does frust. A `--target` build also takes the host slot of its profile (build scripts and proc macros live there), so it
  queues behind other builds of that profile, including builds for other targets. This applies to `build`, `check`,
  `clippy`, `test` and `bench` with a determinable profile; other commands, or a config that relocates outputs, lock the
  whole target directory.
* Builds in the same profile with different commands (`check` then `build`, `-p a` then `-p b`) queue in order.
  Cargo serialises them on its own lock anyway; the queue shows who you are waiting for.
* `frust compact` and `frust gc` take cargo's own per-profile locks without blocking and skip a profile that is
  busy, so running them during builds cannot break a build. `tests/stress.rs` runs eight builds against
  continuous compaction and gc.

## Incremental data and compaction

Plain cargo keeps stale incremental sessions, whole incremental directories of configurations nobody uses
any more, and a hard-linked object alias for every incremental compile that is never removed. Measured over 200
edits with feature toggles, cargo's target grew from 122 MiB to 295 MiB. With automatic compaction frust's target
stays flat (about 83-88 MiB), and the next build after compaction is a no-op. Compaction keeps the newest session of
every crate, so edit builds remain incremental, and compresses files by category: only the categories that
measured a latency cost (proc-macro dylibs, executables) wait an hour before being compressed.
`frust bench --growth` and `frust bench --latency` reproduce these measurements.

## Terminal output

With a terminal on stderr, frust runs cargo with `--message-format json-render-diagnostics` and draws its own
display. `FRUST_RENDER` selects `auto` (default), `ansi`, `plain` (no escape codes, for logs and snapshots)
or `off`. Redirected output is cargo's own, unmodified.

The live line is a full-width Pac-Man bar: he sits at the finished fraction, eats the dots as units complete, and
laps the track while the total is unknown. The total is the unit count of the last similar build (the same
command and arguments, else the same command in this workspace), and the bar holds below 100% until cargo
reports the end. Errors and warnings are restyled in frust's palette with their text and columns unchanged, and
the summary leads with the verdict, then error and warning counts, then the cache counters.

OSC features are auto-enabled only for terminals known to support them (Ghostty, WezTerm, iTerm2, Windows
Terminal, kitty, VS Code, VTE, among others) and can be forced with `FRUST_OSC=all|off|progress,title,links`:

* `OSC 9;4` progress in the tab or taskbar,
* `OSC 2` window title (restored afterwards),
* `OSC 8` hyperlinks from crate names to docs.rs.

`frust ui-demo --snapshot --width 80` renders a scripted build through a headless virtual terminal, which is
also how the display is tested.
