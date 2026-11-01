# How frust works

Details behind the README: cache correctness, disk usage, locking, platforms and the build display.

## How it stays correct

The cache uses a two-step key, the same idea as ccache's direct mode.

1. A *pre-key* is derived from the toolchain, the normalised command line, `CARGO_*` environment, the
   working directory, and the **contents** of every `--extern` library, native library directory and explicitly selected linker executable. Explicit linker names are resolved through the current path; a missing or unreadable executable bypasses caching.
2. Earlier real compiles record, per pre-key, the exact source files they read (from rustc's own dep-info,
   with content hashes) and every environment variable the compile observed (`env!`, `option_env!`,
   tracked env in proc macros). A candidate matches only if every file still has its recorded content hash
   (see [Stamp trust and audits](#stamp-trust-and-audits) for when a file is not re-read) and every observed
   variable still holds.

So an edited dependency, a changed `option_env!` value, a different feature set, profile, flag or compiler
all miss. Nothing is cached when it cannot be done safely:

* incremental compiles keep rustc's own incremental behaviour: on a cache miss the original incremental compile runs unchanged, and its result is then stored (at most 4 entries per unit, reduced to the newest 2 by maintenance on every platform; a hit creates no native incremental session, so the first edit after a clean artifact-only restore rebuilds that crate and its dependents; existing sessions remain available to rustc; `FRUST_CACHE_PRIMARY_INCREMENTAL=off` disables caching of the packages you are building),
* anything with `--print`, unknown `--emit` kinds, stdin input, response files or non-UTF-8 arguments,
* tool chains such as `clippy-driver rustc ...`,
* (workspace packages you are building are cached too when non-incremental, i.e. release builds; `FRUST_CACHE_PRIMARY=off` disables this.)
* (Incrementally compiled units are cached in dev and explicitly incremental optimized profiles, including ThinLTO and fat LTO. Keys omit the session directory and `CARGO_PRIMARY_PACKAGE`, so matching `-p` and whole-workspace builds share artifacts. Optimization, LTO and other compiler settings remain in the key, and incremental entries stay apart from non-incremental entries. Rustc exclusively owns its session directories; cache hits restore completed artifacts. Incremental output can differ from non-incremental output. Release profiles remain non-incremental by default. `FRUST_CACHE_INCREMENTAL=off` disables this.)

Because rustc flags are hashed as opaque text, new flags in future compilers need no support.

New incremental entries record their existing unit identity in metadata for portable retention. Older entries
remain readable and retain the object-name fallback; entries without either identity keep their existing limit.

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
  16 inputs can use up to four workers, subject to the foreground worker budget below.
* A candidate whose cache entry was evicted is skipped before any of its inputs is read.
* On Unix, files of 1 MiB or more under the cargo registry sources or git checkouts, which nobody edits in
  place, that have been untouched for the settle margin are hashed through a memory map. Hashing each file stays
  serial; parallelism comes from the bounded workers across files. Every other file is read, because truncating
  a mapped file faults the reader.
  Windows always reads, because a mapped file cannot be replaced or deleted there and cargo does both to its
  outputs.
* Pre-key hashing of more than eight `--extern` inputs runs in parallel, with ordered key assembly. Workers are capped at four and half the available CPU count; smaller sets stay sequential.
* Foreground hashing, candidate verification, output storage and restores share the same worker policy. With an
  inherited Cargo jobserver, the caller uses its existing slot and each additional worker needs a token acquired
  without waiting. If none is available, work stays on the caller. Without a jobserver, CPU and worker-count caps
  still apply. Each detached store uses one worker without borrowing Cargo tokens; shared slots bound active stores.
* A current-format manifest is not rewritten when its first candidate is unchanged and no row was dropped. Manifests carry a
  format number: new writes use 5, legacy formats 3 and 4 remain readable, and other formats are ignored and rewritten.
* Evicting an entry is one rename into `tmp/evicted` under `FRUST_HOME`, so a concurrent restore fails cleanly
  and recompiles. The remover immediately deletes only its unique retired child. Failed deletions remain queued;
  `frust gc` and daily maintenance retry individual children without an age delay and preserve the queue directory.

## Single-flight and the deferred store

When several processes miss on the same pre-key at once (parallel builds of one project, or two projects sharing a
dependency), only one should compile it.

* **Single-flight.** After a miss, the wrapper takes an exclusive lock on a file named by the pre-key under
  `keylocks/` in `FRUST_HOME`. A process that finds the lock held waits for up to
  `FRUST_KEYLOCK_WAIT_SECS` (default 300), then looks the entry up again and restores it instead of recompiling.
  On Linux, a watcher is created only after contention and wakes on lock-file closure. Every wakeup rechecks
  the OS lock; notifications never grant ownership. Timed retries start at 5 ms and double to 40 ms, providing
  the fallback on unsupported platforms, watcher errors and missed notifications.
  This is fail-open: if the lock file cannot be opened or locked, or the wait runs out, the wrapper compiles
  anyway. Every acquired key lock triggers another lookup, including an immediate acquisition. The lock is
  held until the entry is published. Matching compiler units can share results across profile and target
  directories; different compiler flags keep separate identities.
* Publication, manifest updates, usage refreshes, version-record updates and retirement renames share a short
  cache-wide metadata lock. Compilation, hashing and payload deletion happen outside it. Retirement prunes
  proven-missing candidates from the entry's recorded manifest and removes an empty manifest immediately.
  Live candidates keep their order; legacy or uncertain metadata remains for conservative GC. GC keeps a unit
  record while any referenced cache entry remains and rechecks orphaned records under the same lock before
  removing them. Busy automatic retirements keep the pass incomplete so maintenance retries them.
* On Linux and macOS, a waiting wrapper can lend its implicit slot so independent units compile. This requires
  a verified direct child of frust's supervised Cargo, currently versions 1.89–1.99. A short-lived guardian holds
  Cargo's captured stderr open until it has repaid the loan, including when the wrapper is terminated. Cargo
  therefore retains its accounting for that compiler until repayment. The wrapper releases the key lock before
  reclaiming its slot, then retries the key without blocking. Other callers retain their slots while waiting.
  The guardian starts only on contention and exits after repayment or jobserver EOF; killing the guardian itself
  during a loan is outside this recovery guarantee. Uncertain repayment fails the build before further compilation.
* **Deferred store.** Storing a result means hashing inputs, copying outputs and writing a manifest, none of which
  cargo needs to wait for. After rustc succeeds, the wrapper records the identity of the outputs and inputs,
  writes a job file under `tmp/deferred` and hands it to a background worker. On Linux and macOS, an observed second
  eligible compile attempt starts broker setup while Cargo is active. The broker admits up to eight jobs per shared
  cache home, retaining job metadata without queued output copies. Single-unit builds use the detached helper.
  Workers store an entry only if its files still match and hold the unit key lock until completion. The wrapper
  records the miss once. Broker workers and helpers share execution slots: half the CPU count, between 1 and 4.
* Capacity is reserved before preparing a deferred job, so a saturated queue does not prepare the same outputs twice.
* The store runs inline in the wrapper, as before, when `FRUST_STORE_SYNC=1`, when no slot is free, when the
  result cannot be handed over (for example a directory output, or an incremental compile that printed an
  incremental-compilation warning), for unhashed metadata probes whose outputs may be deleted immediately,
  or when a background handoff cannot complete.

So the entry for a crate can appear a moment after cargo has moved on. A build started in that moment simply
misses or waits on the key lock.

## Cache payload storage

`FRUST_CACHE_COMPRESSION=auto` is the default. New regular-file payloads are compressed with LZ4 only on Linux
when a probe on the cache filesystem finds that reflinks do not work. Reflink-capable cache filesystems and other
platforms keep raw payloads. Files smaller than 64 KiB stay raw, and a compressed file is kept only when it saves
at least 12.5 percent and 4 KiB. Dep-info, stderr and directory bundles stay raw. The codec is pure Rust and needs
no external compression program.

Restores decode bounded 1 MiB blocks and check the exact decoded length, content hash and end of the payload
before replacing the destination. Original file permissions, including executable bits, are preserved. Damaged
or truncated payloads fail the restore, allowing another candidate or the compiler to supply the outputs.

When compression is enabled on a supported filesystem, a `.rmeta` payload can share its matching cached `.rlib` archive. Frust requires an exact byte match inside a checked archive member and records the complete archive and metadata hashes, bounds and permissions. A restore decodes the archive once into separate original outputs and verifies both before publishing them. Unsupported or nonmatching layouts keep ordinary payload storage; compaction requires the same complete proof before treating either output as recoverable.

`FRUST_CACHE_COMPRESSION=off` changes new writes only; existing compressed entries can still be restored. Legacy
raw entries remain readable without conversion. New manifests use format 5 so older readers ignore them, and
encoded entries use a separate key domain from the keys older writers compute.

## Smaller build directories

* Opt-in `FRUST_TUNE=on` builds dependencies without debug info in dev profiles (about a tenth smaller, no
  compile-time cost). `line-tables-only` was measured to be both slower and larger, so it is not used. Skipped
  when your project or cargo config defines any profile.
* Raw artifacts are restored as copy-on-write clones where supported, including APFS (macOS), btrfs and XFS
  (Linux) and ReFS, including Dev Drive (Windows). Elsewhere they are copied. Encoded payloads are restored to
  ordinary compiler artifacts with their original bytes and permissions.
* Cache and target budgets are separate. The cache budget counts the allocation of stored payloads, while the
  target budget counts the restored target tree. Consider both when comparing disk use; shared reflink blocks
  can be counted in both trees when their reported allocations are added.
* Automatic compaction (after successful builds, `FRUST_AUTO_COMPACT`) reclaims superseded artifacts and completed
  stale incremental sessions while preserving current outputs and fresh incremental state. Other profiles stay
  warm for `FRUST_COMPACT_KEEP_DAYS`. Retiring a profile preserves its directory, `deps` and `.fingerprint` roots,
  and Cargo lock files, so processes waiting on those locks keep the same identities. Preserved root modification
  times are restored through opened handles on success, error or unwinding, so an interrupted removal can be retried.
  On macOS compaction also
  compresses idle files with APFS transparent compression (verified byte-for-byte on write) and deduplicates
  identical files as clones; those two steps exist only on macOS.
* Cache coverage is checked while holding the profile locks. APFS clone groups provide a fast proof; elsewhere,
  frust compares complete contents and permissions against raw entries or verifies matching encoded payloads.
  Proofs are reused only while file stamps match and are bounded per pass by time, bytes read and decoded, and
  the number of remembered files. A missing or damaged payload, mismatch or exhausted budget leaves coverage
  unproven, so the existing age rules decide what can be removed. Current outputs and fresh incremental sessions
  keep their existing protection.
* Automatic compaction is synchronous but cheap when there is nothing to do. It runs after Cargo exits and before
  the build lock is handed to a waiter, at background CPU and I/O priority. A completed pass is throttled for
  10 minutes per target; an incomplete pass retries after 30 seconds. Cargo profile locks are taken without
  waiting, so another active build is skipped. Between hourly reassessments, a completed pass can be skipped when
  a change signature (modification times and entry names of a few directories per profile, plus the number of cache
  entries) matches the last complete pass, or when the target grew by less than 64 MiB since then. Once an hour has
  elapsed since the last completed full pass,
  the next due check reassesses idle sessions, expired profiles and settled files even without further growth.
  A busy profile, exhausted work budget or error leaves a pass incomplete without resetting that successful-pass
  time. A persistent memo under `compact-memo/` in `FRUST_HOME` remembers file hashes and files that did not
  compress; compressibility is judged from a sample of two 64 KiB blocks, and a file that would save
  less than 5 percent is left alone. The dedupe and compression stage stops after 2 seconds or 512 MiB of work per
  pass, and the next due pass continues. Explicit `frust compact` has no budget.
* Dead incremental directories (those of a configuration nobody builds any more) are recognised by the codegen
  units they hold. Besides rlib members and object files, frust reads the DWARF string tables of ELF binaries
  (Linux) for rustc's `<source>/@/<codegen unit>` names, so binaries whose objects are gone are still matched.
* `frust gc` removes superseded artifact variants, stale incremental sessions, eligible idle central-target
  payloads and entries over the cache budget (`FRUST_CACHE_MAX_GIB`, default 16). Daily automatic GC covers the
  cache budget, scratch and temporary files, metadata and central-target retirement. It sizes cache entries from
  recorded stored allocations (legacy entries use their existing byte counts), measuring them only when the
  cache is over its budget. An explicit `frust gc` measures every entry.
* A manifest expires only when it and its surviving entries have been idle past the retention period.
  Manifest identity, contents and entry usage are checked again before removal, so activity after planning
  preserves the lookup route to a reused entry.
* A central target becomes eligible for payload retirement after 30 days without activity. If the detached
  collector cannot start, foreground fallback examines at most eight candidates and retires at most two targets,
  with bounded activity scans. The daily detached collector drains eligible targets using a
  complete iterative activity scan that does not follow symlinks, with no fixed per-day target-count limit.
  `FRUST_GC_SYNC=1` performs that drain inline. The cache is scanned once per collection.
  Before deletion, activity and layout are revalidated under an exclusive frust target guard and all
  Cargo profile locks. Expired finalized incremental reuse data is pruned in bounded batches first, so a long build
  history does not require every historical session lock to be held at once. A busy compiler session retains its
  own state and prevents retirement of every program output; other expired reuse data can still be reclaimed.
  Activity and layout are checked again after pruning, and the remaining profiles are admitted together under
  compiler-compatible session guards before program outputs are removed. Target/profile directories, `deps` and
  `.fingerprint` roots, and Cargo lock files remain. Fresh activity, unreadable state and unknown or user-managed layouts
  are preserved.

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
  busy. Maintenance reserves all canonical Cargo profile lock names, including missing ones, so different Cargo
  versions cannot bypass each other's lock files. `tests/stress.rs` runs eight builds against
  continuous compaction and gc.
* Queued builds subscribe to directory changes through kqueue on macOS and inotify on Linux, waking promptly when
  ownership or queue state changes. Notifications are hints: FIFO checks and OS locks still decide ownership.
  Timed polling remains as a fallback (15 ms at the front, 10 ms for followers, up to 40 ms behind the front).
  Uncontended builds create no watcher. Logs are batched every 25 ms, or 10 ms once a follower appears, with
  immediate start/end records and a 64 KiB flush threshold. A follower reads the event feed only when it grows.
* Display-enabled builds request a unit graph only after Cargo survives one 60 ms display tick. Short builds keep their history estimate and avoid the additional Cargo process. Planning is polled alongside compiler
  output, updates the progress estimate when ready, and is cancelled before the build lock is released. It has a
  five-second deadline and never waits for completion after the build. Target selection, tuning and lock scoping
  share an invocation-local snapshot of Cargo configuration files.

## Incremental data and compaction

Completed stale sessions are reclaimed while fresh current state stays available for edits. Dead configurations
can be recognised through their objects and codegen-unit names. Metadata-only check builds have no such object
proof, so their ownership remains unknown. Their completed reuse data can still expire after
`FRUST_COMPACT_INCREMENTAL_IDLE_DAYS` (7 days by default), even without a matching cache entry. A sole newest
unknown directory has no permanent exemption. All entries and modification times must be readable and settled;
working state, object files, symlinks and uncertain timestamps decline this expiry path.

With the defaults, target-budget pressure can reclaim safe metadata-only reuse state after one hour rather than
waiting seven days. Fresh sessions and necessary live outputs remain protected. Cache coverage can permit earlier
reclamation of known superseded outputs, but missing or corrupt cache entries do not disable the ordinary age rules.
The 8 GiB target budget is soft: current outputs, fresh or uncertain state and busy profiles can leave a target
above it. No cleanup policy here imposes a universal hard disk cap, and no work runs while frust is not invoked.

Abandoned `-working` sessions and orphan session lock files have a separate one-hour grace for both modification
and change times. Cleanup requires
compiler-compatible session locks, rechecks file identities and siblings, and preserves recent, held, malformed
or uncertain state. On Unix the guard covers both whole-file and POSIX record locking; Windows uses the compiler's
whole-file locking range. Ordinary Cargo lock files are retained.

Session payload directories are removed before their lock files. If payload removal fails, the remaining data
keeps its lock identity so a later cleanup can validate and retry it safely.

The [latest growth results](benchmarks.md) replay 200 edits across 12 local crates: Cargo used 371.1 MiB, while
frust used 225.5 MiB in total: 174.4 MiB of target allocation, 48.1 MiB of additional cache allocation and
3.1 MiB of other managed storage with automatic compression; values are rounded independently. That
experiment assessed compaction after every build, rather than the default ten-minute interval. Its final unchanged
rebuilds made zero compiler calls and cache events. These workload measurements are not a bound on other projects.

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


## Smart native links

`FRUST_LINK=smart` selects rustc’s supported raw Darwin linker path for unoptimised incremental macOS binaries.
It invokes the current Apple `ld` directly, avoiding the compiler-driver process on each real relink. Rustc still
supplies the target, SDK, library and platform arguments. The standard library remains statically linked, and
executables run directly without a toolchain library search path. This accelerates full native relinks; it does
not claim that Apple’s linker incrementally patches the previous executable.

The policy is opt-in. Explicit linker/flavor/link arguments, non-system compiler drivers, driver customization
environments, custom targets/sysroots, test harnesses and build scripts retain their existing path. A failed
native-linker discovery leaves the original invocation intact; a selected link that fails reports the compiler’s
failure. Effective arguments are prepared before both cache planning and uncached execution, so native and
original link requests have distinct cache keys. Selected linker bytes and identity are validated just like other
inputs, including changes made during a compile.

Smart links use the existing target and rustc incremental directories. Automatic compaction and cache budgets
continue to apply, and no additional persistent linker state accumulates.
