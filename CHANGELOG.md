# Changelog

All notable changes are listed here. The format follows [Keep a Changelog](https://keepachangelog.com/en/1.1.0/).

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
