# How frust works

Details behind the README: cache correctness, disk usage, locking, platforms and the build display.

## How it stays correct

The cache uses a two-step key, the same idea as ccache's direct mode.

1. A *pre-key* is derived from the toolchain, the normalised command line, `CARGO_*` environment, the
   working directory, and the **contents** of every `--extern` library and native library directory.
2. Earlier real compiles record, per pre-key, the exact source files they read (from rustc's own dep-info,
   with content hashes) and every environment variable the compile observed (`env!`, `option_env!`,
   tracked env in proc macros). A candidate matches only if every file still has its recorded content hash
   (see [Stamp trust and audits](#stamp-trust-and-audits) for when a file is not re-read) and every observed
   variable still holds.

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

## Stamp trust and audits

Hashing every source file on every lookup is the main cost of a hit, so a manifest also records a *stamp* next to
each input's content hash: device, inode, size, modification and change times, the time the hash was taken, and
the birth time where the filesystem reports one. A stamp is recorded only if the file had settled (not changed
within 2 seconds before it was hashed), its timestamps are not both whole seconds and its change time is set.

On a lookup, an input whose current stamp equals the recorded one, and which has not changed within that 2 second
margin of the recorded hash time, is accepted without being read. Any difference in identity, size, either
timestamp or birth time (when both sides report one), or a clock that ran backwards, means the content is hashed
as usual.

`FRUST_CACHE_TRUST_STAMPS` selects how far stamps are believed:

* `on` (default): trust stamps, and re-hash some of the trusted inputs on every hit as a check. A hit with four or
  fewer trusted inputs re-hashes all of them; a larger one re-hashes one, chosen pseudo-randomly per run.
* `audit`: every trusted input is compared with a fresh content hash. One CI job runs the test suite this way.
* `off`: always hash content.

A *violation* is an input whose stamp matched but whose fresh hash differs from the recorded one. The candidate is
rejected (so the compile runs), the event is appended to `stamp-trust/violations.jsonl` under `FRUST_HOME`
(capped at 256 KiB, oldest lines dropped), and the volume is quarantined: stamps there are not trusted for 7
days from the latest violation, tracked by a marker file in `stamp-trust/`. `frust doctor` shows a `stamp trust`
check with the mode, any quarantined volumes, the violation count and the number of passed audits.

Stamps are trusted only on filesystems known to keep faithful timestamps. On Unix that is an allow-list of local
filesystems: ext2/3/4, XFS, btrfs, tmpfs, overlayfs, ZFS, F2FS, ramfs, bcachefs, APFS and HFS+. Everything else is
not trusted: network filesystems, shared folders of virtual machines, the FAT family, FUSE, unrecognised
filesystems and any filesystem whose type cannot be determined. On Windows they are trusted only on NTFS and ReFS.
Whole-second timestamps are never trusted either.

Related lookup behaviour:

* Candidates are verified newest first: inputs without a recorded stamp, then by descending recorded modification
  time, because an edited file is the likeliest mismatch and ends a failing check early. A candidate with more than
  16 inputs is verified by four threads.
* A candidate whose cache entry was evicted is skipped before any of its inputs is read.
* On Unix, files of 1 MiB or more under the cargo registry sources or git checkouts, which nobody edits in
  place, that have been untouched for the settle margin are hashed through a memory map, and files of 4 MiB or
  more with rayon's thread pool. Every other file is read, because truncating a mapped file faults the reader.
  Windows always reads, because a mapped file cannot be replaced or deleted there and cargo does both to its
  outputs.
* A manifest is not rewritten when its first candidate is unchanged and no row was dropped. Manifests carry a
  format number (currently 3); one in another format is ignored and rewritten.
* Evicting an entry is one rename into `tmp/evicted` under `FRUST_HOME`, so a concurrent restore fails cleanly
  and recompiles; `frust gc` and the daily maintenance delete such entries once they are an hour old.

## Single-flight and the deferred store

When several processes miss on the same pre-key at once (parallel builds of one project, or two projects sharing a
dependency), only one should compile it.

* **Single-flight.** After a miss, the wrapper takes an exclusive lock on a file named by the pre-key under
  `keylocks/` in `FRUST_HOME`. A process that finds the lock held polls it (5 ms, doubling to 40 ms) for up to
  `FRUST_KEYLOCK_WAIT_SECS` (default 300), then looks the entry up again and restores it instead of recompiling.
  This is fail-open: if the lock file cannot be opened or locked, or the wait runs out, the wrapper compiles
  anyway. The lock is held until the entry is published.
* **Deferred store.** Storing a result means hashing inputs, copying outputs and writing a manifest, none of which
  cargo needs to wait for. After rustc succeeds, the wrapper records the identity of the outputs and inputs,
  writes a job file under `tmp/deferred` and starts a detached helper (`frust-wrap --deferred-store <job>`, a
  hidden argument) at background priority, then returns. The helper stores the entry only if those files still
  match, prints nothing, always exits 0, and holds the key lock until it finishes. It does not record a second
  cache event; the wrapper already recorded the miss. A bounded pool of slot locks (half the CPU count, between 1
  and 4) limits how many helpers run at once.
* The store runs inline in the wrapper, as before, when `FRUST_STORE_SYNC=1`, when no slot is free, when the
  result cannot be handed over (for example a directory output, or an incremental compile that printed an
  incremental-compilation warning) or when the helper cannot be started.

So the entry for a crate can appear a moment after cargo has moved on. A build started in that moment simply
misses or waits on the key lock.

## Smaller build directories

* Opt-in `FRUST_TUNE=on` builds dependencies without debug info in dev profiles (about a tenth smaller, no
  compile-time cost). `line-tables-only` was measured to be both slower and larger, so it is not used. Skipped
  when your project or cargo config defines any profile.
* Reused artifacts are copy-on-write clones on APFS (macOS), btrfs and XFS (Linux) and ReFS, including Dev Drive
  (Windows). Elsewhere they are copied.
* Automatic compaction (after successful builds, `FRUST_AUTO_COMPACT`) removes artifacts no current fingerprint references, keeps the newest incremental session of every crate (so edit builds stay incremental) and keeps the other profile warm for `FRUST_COMPACT_KEEP_DAYS`. Cargo sees every unit as fresh afterwards, and anything removed that the compile cache holds comes back as a restore. On macOS it also compresses idle files with APFS transparent compression (verified byte-for-byte on write) and deduplicates identical files as clones; those two steps exist only on macOS.
* Automatic compaction is synchronous but cheap when there is nothing to do. It runs after the build's summary and
  lock hand-off, on a thread at background CPU and I/O priority, and at most once every 10 minutes per target
  directory. It is skipped when a change signature (modification times and entry names of a few directories per
  profile, plus the number of cache entries) matches the last complete pass, or when the target grew by less
  than 64 MiB since then. A persistent memo under `compact-memo/` in `FRUST_HOME` remembers file hashes and files
  that did not compress; compressibility is judged from a sample of two 64 KiB blocks, and a file that would save
  less than 5 percent is left alone. The dedupe and compression stage stops after 2 seconds or 512 MiB of work per
  pass, and the next due pass continues. Explicit `frust compact` has no budget.
* Dead incremental directories (those of a configuration nobody builds any more) are recognised by the codegen
  units they hold. Besides rlib members and object files, frust reads the DWARF string tables of ELF binaries
  (Linux) for rustc's `<source>/@/<codegen unit>` names, so binaries whose objects are gone are still matched.
* `frust gc` removes superseded artifact variants, stale incremental sessions, idle target directories and
  entries over the cache budget (`FRUST_CACHE_MAX_GIB`, default 16). A lighter version runs automatically at most
  once a day, started by the next `fcargo` command in a detached background process (`FRUST_GC_SYNC=1` runs it
  inline). It covers the cache budget, scratch and temporary files and metadata, not idle target directories or
  stale variants, and it sizes cache entries from their recorded byte counts, measuring them only when the cache is
  over its budget. An explicit `frust gc` measures every entry.

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
* The hand-over from one build to the next is kept short: waiters poll every 15 ms at the front of the queue, the
  holder's build log is written in batches (every 25 ms), live redraws are limited to about 40 per second, and a
  follower reads the event feed only when the file has grown.

## Incremental data and compaction

Plain cargo keeps stale incremental sessions, whole incremental directories of configurations nobody uses
any more, and a hard-linked object alias for every incremental compile that is never removed. Measured over 200
edits with feature toggles, cargo's target grew from 122 MiB to 295 MiB. With automatic compaction frust's target
stays flat (about 83-88 MiB), and the next build after compaction is a no-op. Compaction keeps the newest session of
every crate, so edit builds remain incremental, and compresses files by category: only the categories that
measured a latency cost (proc-macro dylibs, executables) wait an hour before being compressed.
`frust bench --growth` and `frust bench --latency` reproduce these measurements.

## Platforms

Everything that differs between operating systems lives in `src/sys/`, with a Unix backend and a Windows backend
behind one interface. CI fails if platform calls appear elsewhere (the macOS-only APFS module in `src/compact.rs`
is the one exception). macOS, Linux and Windows are all supported natively. Windows support is new: the test suite
runs on Windows in CI, but it has had much less real-world use than the Unix ports.

| Concern | Unix | Windows |
|---|---|---|
| Build and key locks | advisory whole-file locks | `LockFileEx` on a sentinel byte, so file contents stay readable |
| Exit status after Ctrl-C | the signal-based status (128 plus the signal) | cargo's Ctrl-C exit status is mapped to exit code 130 |
| Running cargo | the frust process is replaced by cargo where possible | frust runs cargo as a child inside a job object that kills the tree if frust is terminated |
| Permissions | POSIX mode bits | not applied; the state directory must be owned by you, and access follows the ACLs it inherits |
| Copy-on-write clones | APFS, btrfs, XFS | volumes that support block cloning (ReFS, including Dev Drive) |
| APFS compression and dedupe | macOS only | not available |
| Stamp trust | the allow-list of local filesystems above | NTFS and ReFS only |
| Large-file hashing | memory map for registry and git sources, plain reads otherwise | plain reads |
| Dead incremental directories | attributed by object links (macOS) or DWARF codegen-unit names (Linux) | an older directory of a crate is treated as superseded by a newer sibling, as the linker leaves no proof of ownership |
| Detached helpers | new session, locks inherited | detached process in its own process group; handles are not inherited, so a helper takes its locks again by path |

On Windows, frust also enables virtual terminal processing on the console so the ANSI display works. Which cargo to
run, `RUSTC_WRAPPER` handling and the cache itself are the same everywhere.

## Terminal output

With a terminal on stderr, frust runs cargo with `--message-format json-render-diagnostics` and draws its own
display. `FRUST_RENDER` selects `auto` (default), `ansi`, `plain` (no escape codes, for logs and snapshots)
or `off`. Redirected output is cargo's own, unmodified.

The live line is a fine progress bar that fills in half-cell steps, with the percent, the exact unit count, the elapsed
time and the cache hits and misses; below it, each running unit has its own timer aligned at the right edge, and a timer
turns yellow after five seconds. For `build`, `check`, `run` and `test`, frust asks cargo for its unit graph under the
build lock (about a tenth of a second, bounded to five) and uses that count as the total, so the percent is exact and
the same value is sent to the terminal as OSC 9;4 progress. If the plan fails, the total falls back to the unit count of
the last similar build (the same command and arguments, else the same command in this workspace), and without either the
bar shows a sweeping segment instead of a percent. The bar holds below 100% until cargo reports the end. Errors and warnings are restyled in frust's palette with their text and columns unchanged, and
the summary leads with the verdict, then error and warning counts, then the cache counters.

OSC features are auto-enabled only for terminals known to support them (Ghostty, WezTerm, iTerm2, Windows
Terminal, kitty, VS Code, VTE, among others) and can be forced with `FRUST_OSC=all|off|progress,title,links`:

* `OSC 9;4` progress in the tab or taskbar,
* `OSC 2` window title (restored afterwards),
* `OSC 8` hyperlinks from crate names to docs.rs.

`frust ui-demo --snapshot --width 80` renders a scripted build through a headless virtual terminal, which is
also how the display is tested.

## Updates

After a successful build on an interactive terminal, frust may print a one-line notice that a newer release exists.
The release check behind it runs at most once a day, in a detached background process, so it never delays a build;
it is skipped when stderr is not a terminal, when `CI` is set, with `FRUST_BYPASS`, with `FRUST_UPDATE_CHECK=off` and
after `frust self-update --disable`. The command is `frust self-update` and not `frust update`, because `cargo
update` must keep working through `fcargo`.

`frust self-update` flags: `--yes` (skip the confirmation), `--check` (print the result and exit with code 10 when an
update exists), `--version vX.Y.Z` (install that release), `--disable` and `--enable` (turn the background check off
and on). An install downloads the release archive for the platform, checks its SHA-256 against the published
checksum, extracts it, runs the staged `frust --version` to confirm the version, backs up the eight installed
binaries and replaces each atomically, restoring the backups if any replacement fails. A copy installed with
`cargo` or Homebrew (and Scoop or the Windows store) prints the matching update command instead of replacing files.
