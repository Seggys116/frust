# Changelog

All notable changes are listed here. The format follows [Keep a Changelog](https://keepachangelog.com/en/1.1.0/).

## [Unreleased]

* 20 November 2026: the interface-hash cutoff was judged on the full paired Linux cohort and not kept. Build-time gates passed and the runtime aggregate and stripped-size gates failed, because the three application binaries matched stock. No candidate has passed every frozen Linux budget. See `qualification/GATE-REPORT.md` and `docs/benchmarks.md`.
* 20 November 2026: prelink-span reuse (`frust-0ef1bc386-prelinkspan1`) was judged on the same cohort and not kept. The leaf build budgets passed. Disk, the runtime aggregate, and stripped size failed. The three application binaries matched stock. See `qualification/GATE-REPORT.md` and `docs/benchmarks.md`.
* 20 November 2026: an x86-64-v3 codegen preflight on that compiler, with the hotfreq16 flags and opt-in RELR, was not kept. Oxipng's median ratio was 0.94419073. The three-workload runtime geomean was 0.98290005, and rustfmt's median was 1.01459543. Stripped size beat 0.97 only while RELR stayed on. No full judge was run. See `qualification/GATE-REPORT.md` and `docs/benchmarks.md`.
* 20 November 2026: a ripgrep build with the machine outliner and `-C force-unwind-tables=no`, and without RELR, was not kept. Stripped size was 4010848/4024992 (0.99648596). That ratio cannot carry a 0.97 size geomean. Oxipng, rustfmt, and the full judge were not run. See `qualification/GATE-REPORT.md` and `docs/benchmarks.md`.

## [0.5.0] - 2026-11-20

* Native Linux x86-64 fCargo builds now default to incremental compilation with 16 codegen units for local packages using fat LTO at opt level 3. Explicit Cargo profile and incremental settings take precedence.

Compiler rewrite. fRust is now a Rust 1.100.0 toolchain built from pinned upstream sources, with a patched Cargo (`fcargo`), `frustc`, the unit and build script cache, central targets and `frust-install`. The earlier 0.4.x line is kept on the `legacy-0.4` branch. Toolchains are built for Linux x86-64, macOS on Apple Silicon and macOS on Intel by the release workflow. See `docs/benchmarks.md` and `qualification/CANDIDATE.md`.

### Added

* Started the fRust compiler rewrite and retired the previous build accelerator from the active branch. The entries for 0.3.0 and earlier describe that accelerator.
* `fcargo`, which runs Cargo and sets `RUSTC` to a sibling `frustc` when unset. `FRUST_PROGRESS` selects plain, static or interactive progress, and plain, dumb-terminal, quiet and explicit JSON invocations hand over to Cargo. `FRUST_PROGRESS_COLOR`, `FRUST_BUILD_REPORT` and `FRUST_CARGO` are also read.
* `frust-install`, which installs, updates, stages, rolls back and uninstalls a toolchain. With no command it downloads the latest release for the host from GitHub, verifies the sha-256 and installs it, and `frust-install update` replaces it when the release id changes. It can also install a local archive and sha-256 file. The checksum is verified before extraction and the `current` symlink changes only after the tree is complete. The release workflow builds toolchains for Linux x86-64 and for macOS on Apple Silicon and Intel; the macOS ones are unverified until a release run passes on them.
* `frust-gates`, which recomputes the frozen Linux budgets from raw paired samples.
* Cargo patch for central target directories and a user JSON config file (`config.json` with `version`, `target_root` and `cache_root`), active under `fcargo`. Outputs go to a per-user directory named from the toolchain id and the workspace path. An `artifacts` cache directory with a marker file is created.
* Cargo patch (`0002-frustc-content-fingerprint.patch`) that compares content checksums instead of timestamps when Cargo's compiler path is named `frustc`.
* Eligible `fcargo check` invocations on x86-64 GNU/Linux share verified unit outputs by default. Independent work can run while a peer compiles a shared unit. A stalled check releases its reader locks and retries serially. Set `FRUST_UNIT_JOIN=off` to use ordinary scheduling.
* 19 November 2026: `FRUST_HOME` is honoured again. It replaces the target and artifact roots for one invocation, ahead of `config.json`, as in the 0.4.x line. The embedded copy of `paths.rs` in the central-target patch is updated with it.
* 19 November 2026: Cargo patch `0006-frust-lazy-libgit2.patch`. libgit2, and the system CA bundle load that comes with its TLS setup, now initialize on first git use instead of at the start of every Cargo invocation. Git dependencies, `cargo new`, `cargo fix` and proxy detection through git config still initialize it first, and the owner-validation setting is applied before the first repository is opened.
* 19 November 2026: Cargo patch `0008-frust-direct-entry.patch`. In the packaged toolchain `fcargo` is a hard link of the patched `cargo`, so plain non-terminal invocations skip one process launch. The terminal progress launcher is installed as `fcargo-ui`, and interactive terminals, static progress, an explicit `FRUST_CARGO` and a `+toolchain` argument still go through it. The direct entry is compiled for Linux and macOS.
* 19 November 2026: Cargo patch `0007-frust-unit-cache.patch`, a verified content-addressed unit artifact cache. Finished rustc outputs of registry, git and directory-source library and proc-macro units are stored under `units/v1` in the artifact cache root and restored in place of rustc on a verified hit. On by default under `fcargo`. Heavy build script runs of registry and git packages (3000 ms by default) are cached in the same store, keyed on their environment, tools and system directories and bound to the same target path. Controls are `FRUST_UNIT_CACHE`, `FRUST_UNIT_CACHE_MAX_BYTES`, `FRUST_UNIT_CACHE_SCRIPT_MIN_MS` and `FRUST_UNIT_CACHE_DEBUG`. Any mismatch or store problem compiles normally. Covered by 86 unit tests in the two cache modules and the 23-case `qualification/prove-unit-cache.py` probe, and exercised with the existing cooperative-build and managed-retention proof scripts. Linux x86-64 is the only verified host. The macOS code paths are compiled in and unverified. Measured results are not recorded yet. See `docs/how-it-works.md` for the key, limits and fallbacks.
* Compiler patch for post-LTO object reuse and RELR packing, kept as the candidate `frust-cg1` and not qualified. One ripgrep leaf sample ran at 0.655 of stock wall time, and single samples of the syn leaf, regex leaf and fd holdout were slower than stock. Both pieces are opt-in through `FRUST_EXPERIMENTAL_CODEGEN_CACHE=1` and `FRUST_EXPERIMENTAL_RELR=1`. Default compilation and linking retain upstream behavior.
* Continuous integration for the Rust workspace, the shell scripts, the runtime corpus and the pinned upstream patches, and manual `Toolchains` and `Benchmark` workflows that build the control and fRust stage-2 toolchains and run the paired qualification samples and release gates on GitHub's Linux x86-64 runners. The workload commits and the three generated `Cargo.lock` files are recorded in `qualification/cohorts.lock` and `qualification/locks/`.
* Qualification protocol, frozen workload cohort, baselines, edit scripts, runtime corpus, a parity proof script and compatibility record under `qualification/`.

### Changed

* The workspace packages are `frust-rs` (the installer binaries, replacing the 0.4.0 accelerator on crates.io) and `frust-rs-core`, version 0.5.0, licensed MIT to match `LICENSE`. `cargo install frust-rs` installs `frust-install`, `fcargo` and `frust-gates`. Pushing a `v*` tag runs the release workflow, which publishes the GitHub release and then the crates.

* Cargo skips checksumming excluded dependencies and treats dependencies saved during compiler invocation as dirty on the next build. Upgrading invalidates the earlier dependency checksum records.
* Qualification requires the complete frozen cohort and rejects missing tail series and invalid timings. The parity fixture now compiles and links a native archive and tests an edit between compiler completion and Cargo's fingerprint write.

### Dropped

* Pre-optimization fat-LTO cache. The full paired gate failed the holdout, runtime and stripped-size budgets, with the runtime and size gates also short of three workloads.
* Two-thread fat-LTO machine codegen. One fd holdout pair ran at 21.19s against 15.90s with 1.62 times the peak RSS, so the paired gate was not run.
* Fat-LTO pre-link at opt-level 1, and at opt-level 0 with an x86 machine outliner. Single samples did not reach the 0.80 leaf budget, and the opt-level 0 variant used about 2.11 times the control's peak RSS.
* Safe identical-code folding (`--icf=safe`). The ripgrep output was byte-identical to stock.
* Hot-cold splitting and scheduling retargets (`-Z tune-cpu`) were checked with the stock compiler and not adopted.

### Hardening

* Hardening fixes to the tools and documentation are in progress on this branch. The commit history has the specifics.

## Earlier accelerator releases

The sections for 0.3.0, 0.2.0, 0.1.1 and 0.1.0 describe the earlier fRust build accelerator, a compile-cache wrapper that is no longer in the active branch. They do not describe the compiler toolchain.

## [0.3.0] (earlier accelerator)

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
* Windows MSVC cache hits restore unsuffixed PDB companions and bypass older incomplete entries.
* macOS store transport validates inherited socket roles and closes every rejected transferred descriptor.
* Target maintenance preserves files owned by pending stores and retries their cleanup after publication.

## [0.2.0] (earlier accelerator)

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

## [0.1.1] (earlier accelerator)

### Fixed

* The terminal progress signal (OSC 9;4) now carries an exact percent. When the display is on, frust asks cargo for its
  unit graph under the build lock (about 0.1 s, bounded to 5 s, and skipped for subcommands whose stream does not match
  it), so the bar fills from 0 to 100 instead of looping or stopping at an old estimate.
* The live display is redrawn as a fine line bar with half-cell steps, percent, exact unit count, elapsed time and cache
  hits, and unit timers aligned at the right edge. The animated Pac-Man bar is gone.

## [0.1.0] (earlier accelerator)

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
