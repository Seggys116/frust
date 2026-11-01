# Changelog

All notable changes are listed here. The format follows [Keep a Changelog](https://keepachangelog.com/en/1.1.0/).

## [0.3.0]

### Changed

* Concurrent cold builds share matching compiler units while retaining independent profile locks and original compiler flags.
* Concurrent benchmarks select dev, release, thin or fat profiles with native compiler-flag and executable-byte checks.
* Explicitly incremental optimized profiles share completed artifacts, including ThinLTO and fat LTO, while rustc retains its native sessions.
* Cache format 5 stores verified metadata ranges inside their backing archives instead of duplicating metadata payloads.
  Formats 3 and 4 remain readable. Public cache metadata types gain fields and an encoding variant.
* Deferred stores use a bounded broker queue and shared execution slots on Linux and macOS, with the existing helper
  and inline fallbacks. Multiple cache misses start broker setup during compilation; single-unit builds use the helper.
* Compression scratch buffers follow artifact size while retaining the existing encoded frames and verification rules.
* Output commitment hashing reuses bounded thread-local scratch storage instead of allocating a fresh block per file.
* Fixed-persona launchers specialize shared dispatch code, reducing executable size while preserving renamed entrypoints.

### Fixed

* Native incremental session maintenance preserves the compiler's current completed session and held working sessions.
* Retired cache entries prune their manifest and unit references; interrupted atomic records remain eligible for cleanup.
* Garbage collection preserves execution-slot lock identities so concurrent stores retain their global worker bound.
* Metadata proof generation reuses verified encoder input and avoids redundant archive scans and temporary writes.
* Native linker diagnostics retain actual invocation evidence and reject unsupported or ambiguous measurements.

## [0.2.0]

### Added

* `FRUST_CACHE_COMPRESSION=auto` stores compressible cache payloads as bounded LZ4 blocks on Linux filesystems
  without copy-on-write clones. `off` writes raw payloads; both settings can restore existing raw and compressed
  entries. Cache budgets account for the payloads' stored allocation.
* Opt-in `FRUST_LINK=smart` uses Apple's native linker directly for eligible macOS incremental development binaries,
  retaining static Rust linkage and the current incremental state. Explicit linker settings, custom drivers, tests,
  release builds and unsupported invocations keep their original link path.
* Release benchmarks cover ordinary release, ThinLTO and full/fat LTO, with real function-body edits, alternating
  build order, raw samples, actual compiler invocation audits, same-path binary identity and runtime output checks.
  Growth benchmarks compare combined target and cache allocation with compression enabled and disabled. Native-link
  profiling measures real application edits and separates the current binary's native-link phase from total build time.

### Changed

* Encoded cache manifests use format 4 so older readers do not restore compressed payloads as compiler outputs.
  Existing format-3 raw manifests remain readable.
* Completed metadata-only incremental reuse data with unknown ownership can expire after the configured idle
  retention (7 days by default), without cache coverage or a permanent newest-directory exemption. Target pressure
  can reclaim safely aged metadata reuse state after an hour with the defaults; fresh and uncertain state stays.
* Daily automatic GC retires eligible central-target payloads after 30 idle days. Foreground sampling is bounded;
  the detached collector drains eligible targets with a complete activity scan, and `FRUST_GC_SYNC=1` drains inline.
  Target/profile directories, `deps` and `.fingerprint` roots, and Cargo lock files remain; unknown or user-managed layouts
  are skipped. Cache and target budgets
  remain separate; the target budget is soft and does not impose a universal disk cap.
* Release publication requires the tag to match the package version and the latest main-branch push CI run for the
  exact release commit to have completed successfully before any release binaries are built.

### Performance

* Bounded, jobserver-aware workers hash source and dependency inputs with ordered key assembly and balance uneven
  input sizes. Small input sets stay sequential, and large-file hashing no longer starts a separate Rayon pool.
* Compaction creates cache-coverage indexes only when needed and reuses memoized file information. Deferred stores
  reserve helper capacity before preparing outputs, avoiding duplicate preparation when the helper pool is full.
* Queued builds wake on Linux inotify and macOS kqueue notifications, retaining FIFO checks and timed polling as
  fallbacks. Follower logs flush in bounded batches instead of flushing every record.
* Display-enabled builds defer unit-graph planning until the first display tick and poll it alongside compiler output.
  Short builds avoid the extra Cargo process, and planning is cancelled before the build lock is released. Target
  selection, tuning and lock scoping share an invocation-local snapshot of Cargo configuration.

### Fixed

* Restored output timestamps use the Unix filesystem clock, preventing fast cache hits from making Cargo's subsequent
  dependency and build-script timestamps older than their restored inputs.
* Successful metadata-only compiler probes can cache exact un-hashed output names; failed capability probes continue
  to run the compiler. Current inputs and stored payloads are checked before a cached result is accepted.
* Cache keys include explicitly selected linker executables and native inputs, with unreadable inputs bypassing the
  cache and changes during compilation preventing storage.
* Build locks are explicitly released even while forked processes retain cloned descriptors, so process startup cannot
  prolong a completed build's ownership.
* Due compaction runs after Cargo exits and before the build-lock handoff, so queued builds cannot continually take
  the profile before cleanup. Incomplete passes retry after 30 seconds instead of the completed-pass 10-minute
  interval; retries and cheap skips preserve the last successful pass time for hourly age reassessment.
* Crashed incremental working sessions and orphan session locks can be reclaimed after a one-hour grace using
  compiler-compatible guards and identity revalidation. Recent, held, malformed and uncertain state is retained.
* Idle-target cleanup first prunes expired finalized incremental reuse data in bounded batches, allowing progress
  through long build histories. Held compiler sessions protect their state and all program outputs; remaining
  program-output retirement requires joint admission of every profile. Session payloads are removed before locks,
  and preserved structural roots and activity times allow safe retries after partial removal.
* Maintenance reserves every canonical Cargo profile lock name, including absent alternatives, and profile
  retirement preserves their identities. Structural symlinks and unsafe central paths decline cleanup;
  nested native-output symlinks do not prevent unrelated cleanup and are not followed outside the managed tree.
* Benchmark no-op checks require zero actual compiler calls, Cargo compile notices and cache events. Warm restores
  distinguish declared Cargo-unit compilations from build-script capability probes, and unavailable audits remain
  unknown rather than being reported as zero. Runtime output checks cover every measured executable launch.

## [0.1.1]

### Fixed

* The terminal progress signal (OSC 9;4) now carries an exact percent. When the display is on, frust asks cargo for its
  unit graph under the build lock (about 0.1 s, bounded to 5 s, and skipped for subcommands whose stream does not match
  it), so the bar fills from 0 to 100 instead of looping or stopping at an old estimate.
* The live display is redrawn as a fine line bar with half-cell steps, percent, exact unit count, elapsed time and cache
  hits, and unit timers aligned at the right edge. The animated Pac-Man bar is gone.

## [0.1.0]

### Added

* Shared compile cache, central target directories, cooperative build lock and build display.
* Native Windows support alongside macOS and Linux, through a platform layer in `src/sys/` with Unix and Windows
  backends: build locks with `LockFileEx` on a sentinel byte, Ctrl-C reported as exit code 130, a job object around
  cargo, ANSI virtual terminal processing, no POSIX mode bits (access follows inherited ACLs) and copy-on-write
  clones on ReFS and Dev Drive. Windows support is new and verified by CI.
* Prebuilt binaries for Linux, macOS and Windows, `install.sh`, `install.ps1` and `cargo binstall` support.
* Continuous integration on all three platforms, an MSRV build and a job that runs the tests with
  `FRUST_CACHE_TRUST_STAMPS=audit`.
* Stamp-validated direct mode: manifests record a file stamp per input, and an input whose stamp is unchanged is
  accepted without being read. `FRUST_CACHE_TRUST_STAMPS` selects `on` (default, trusted inputs are randomly
  audited), `audit` (every trusted input is compared with a fresh content hash) or `off`. A stamp that lies is logged
  under `stamp-trust/`, makes the compile run and quarantines the volume for 7 days. Network, FAT, FUSE and unknown
  filesystems are never trusted; on Windows only NTFS and ReFS are. `frust doctor` gains a `stamp trust` check.
* Per-pre-key single-flight: processes missing on the same unit take a lock file under `keylocks/`, and a waiter
  restores the finished entry instead of recompiling. It fails open, and `FRUST_KEYLOCK_WAIT_SECS` (default 300)
  bounds the wait.
* Deferred store: a detached helper process finishes storing a compile result after rustc exits, so cargo does not
  wait for it. `FRUST_STORE_SYNC=1` stores inline.
* `frust self-update` with `--yes`, `--check` (exit code 10 when an update exists), `--version`, `--disable` and
  `--enable`. It downloads the release archive, verifies its SHA-256, confirms the staged binary's `--version` and
  replaces all eight binaries atomically with rollback. Copies installed with cargo or Homebrew get the matching
  update command instead. The command is not `frust update` so that `cargo update` keeps working.
* A one-line update notice after a successful build, from a release check that runs at most once a day in a detached
  process, only on an interactive terminal and never in CI or with `FRUST_BYPASS`. `FRUST_UPDATE_CHECK=off`
  disables it. `FRUST_UPDATE_API_URL` and `FRUST_UPDATE_DOWNLOAD_BASE` point the updater at a mirror, for mirrors and
  tests.
* `FRUST_GC_SYNC=1` runs the daily automatic gc inline.
* `frust bench --runs N` (default 5, 3 with `--quick`, minimum 3), median and min-max spread per row, a warm-up
  build, alternating tool order, separate gc and compaction rows, a display-on row and machine context in the report.

### Changed

* Automatic compaction stays synchronous but runs at background CPU and I/O priority, is skipped when a change
  signature shows nothing changed since the last complete pass, remembers file hashes and incompressible files in
  `compact-memo/`, judges compressibility from a sample and stops after a work budget (2 seconds or 512 MiB per
  pass), leaving the rest to the next pass.
* The daily automatic gc runs in a detached background process and sizes cache entries from their recorded byte
  counts, measuring them only when the cache is over its budget. An explicit `frust gc` still measures every entry.
* The manifest format is now 3. Manifests in other formats are ignored and rewritten, and their cache entries stay
  reachable for gc.
* `frust bench` exits with code 1 when a byte-identity or output check fails.
* Evicting a cache entry is a single rename; its files are deleted later.

### Fixed

* Compaction on Linux recognises dead incremental directories of binaries by reading the codegen-unit names in the
  DWARF data of ELF files, so they are removed even though their objects are gone.
* On Windows, binaries share one fingerprint directory across feature sets, so only the older incremental directories of a
  crate are treated as superseded.
* The toolchain probe runs one query before the parallel ones. Before, the first use of an uninstalled toolchain named
  by `rust-toolchain.toml` made rustup auto-install it from several processes at once, which left it without `cargo`.
* Dependency files for profile directories whose path contains spaces are rewritten and restored correctly.
* A non-UTF-8 environment variable no longer aborts the wrapper.

### Performance

* Large files in the immutable registry and git source trees are hashed through a memory map on Unix (with rayon for
  files of 4 MiB or more). Everything else is read, because an editor truncating a mapped file in place would fault.
* Candidates are verified newest-modified first, in parallel for large ones, and a candidate whose cache entry is
  gone is skipped before any input is hashed.
* Manifests are not rewritten when nothing changed.
* The lock hand-off between builds is faster: quicker waiter polling, a build log written in batches, coalesced
  display redraws and an event feed read only when it has grown.
