# Configuration

For switches shown as `on` or `off`, the values `off`, `0`, `false` and `no` turn them off, and `on`, `1`, `true` and `yes` turn them on. Anything else leaves the default.

| Variable | Default | Meaning |
|---|---|---|
| `FRUST_HOME` | platform cache dir + `/frust` | All state lives here |
| `FRUST_CACHE` | `on` | Compile cache |
| `FRUST_CACHE_COMPRESSION` | `auto` | New cache writes use LZ4 on Linux when a reflink probe fails; `off` writes raw payloads. Both settings can read existing raw and compressed entries. See [cache payload storage](how-it-works.md#cache-payload-storage) |
| `FRUST_CACHE_PRIMARY` | `on` | Cache workspace packages too (non-incremental builds only) |
| `FRUST_CACHE_PRIMARY_INCREMENTAL` | `on` | Cache incrementally compiled workspace outputs. Clean builds and branch switches can restore matching outputs; the next edit may need to rebuild incremental state. Storing changed states adds cache work; see [benchmarks](benchmarks.md). |
| `FRUST_COMPACT_INCREMENTAL_IDLE_DAYS` | `7` | Idle incremental reuse data retention. Metadata-only state with unknown ownership can expire without cache coverage; fresh, active and unreadable state is protected. See [cleanup rules](how-it-works.md#incremental-data-and-compaction) |
| `FRUST_BENCH_MAX_SCRATCH_MIB` | `4096` | Stops `frust bench` when its scratch directory passes this physical size |
| `FRUST_CACHE_INCREMENTAL` | `on` | Cache incremental compiles of non-primary path dependencies in dev builds |
| `FRUST_CACHE_TRUST_STAMPS` | `on` | How far recorded file stamps are believed on a lookup. `on` accepts an input whose stamp is unchanged without reading it and re-hashes some of those inputs on every hit (all of them when there are four or fewer, otherwise one chosen at random). `audit` re-hashes every such input. `off` (or `0`, `false`, `no`) always hashes content. Any other value means `on`. A stamp that matched while the content differed is a violation: it is logged under `stamp-trust/` in `FRUST_HOME`, the compile runs, and stamps are not trusted on that volume for 7 days. Stamps are never trusted on network, FAT or FUSE filesystems or ones of unknown type, and on Windows only on NTFS and ReFS. `frust doctor` reports the state. See [how it works](how-it-works.md#stamp-trust-and-audits) |
| `FRUST_KEYLOCK_WAIT_SECS` | `300` | How long a compile waits for another process compiling the same unit before compiling anyway. A waiter that is released looks the entry up again and restores it. Whole seconds; `0` does not wait |
| `FRUST_STORE_SYNC` | `off` | `on` stores each compile result inside the wrapper before it returns. By default a detached helper finishes the store after rustc exits so cargo does not wait for it |
| `FRUST_GC_SYNC` | `off` | `on` runs the daily automatic gc inline, including draining eligible idle central targets, and can delay the invoking command. By default the daily collector runs detached |
| `FRUST_CACHE_MAX_GIB` | `16` | Cache-store allocation budget enforced by `frust gc` and the daily automatic gc; encoded payloads count at their stored allocation |
| `FRUST_AUTO_COMPACT` | `on` | Compact central targets after successful builds, before handing the build lock to a waiter. Completed passes are throttled for 10 minutes; incomplete passes retry after 30 seconds |
| `FRUST_COMPACT_KEEP_DAYS` | `14` | How long an unused profile's artifacts are kept before auto-compaction removes them |
| `FRUST_TARGET_MAX_GIB` | `8` | Soft per-target-directory budget, separate from the cache store. Old superseded artifacts and safely aged reuse state are eviction candidates; necessary live outputs and protected state can exceed it |
| `FRUST_TARGET` | `central` | `local` leaves `CARGO_TARGET_DIR` alone |
| `FRUST_TUNE` | `off` | Opt-in dependency debuginfo reduction |
| `FRUST_LINK` | `off` | `smart` invokes Apple’s native linker directly for eligible macOS host dev binaries, avoiding the default compiler-driver process. Keeps static Rust linkage and current incremental state. Explicit linker options, custom drivers, cross targets, tests and release builds keep their existing link path |
| `FRUST_LOCK` | `on` | Queued build lock |
| `FRUST_LOCK_TIMEOUT_SECS` | none | Give up waiting for the lock |
| `FRUST_RENDER` | `auto` | `auto`, `ansi`, `plain`, `off` |
| `FRUST_OSC` | `auto` | `auto`, `off`, `all`, or a list |
| `FRUST_DEBUG` | `off` | Print diagnostics from the compile wrapper (cache decisions, timings) and from automatic compaction to stderr |
| `FRUST_UPDATE_CHECK` | on | `off` (or `0`, `false`, `no`) stops the daily release check and the update notice. `frust self-update --disable` does the same with a marker file in `FRUST_HOME`, and `--enable` removes it |
| `FRUST_UPDATE_API_URL` | GitHub's latest-release API for frust | Where `frust self-update` looks up the release tag. For mirrors and tests; setting either update URL variable also allows non-https and `file` URLs |
| `FRUST_UPDATE_DOWNLOAD_BASE` | `https://github.com/Seggys116/frust/releases/download` | Base URL of the release archives and their `.sha256` files. For mirrors and tests |
| `FRUST_CARGO`, `FRUST_RUSTC` | PATH lookup | Use a specific binary |
| `FRUST_BYPASS` | unset | Run cargo completely untouched |

An explicit `CARGO_TARGET_DIR`, `--target-dir`, or a `target-dir` in your cargo config is always respected.

Automatic target retirement applies to frust-managed central targets after 30 days of inactivity. It rechecks
activity while holding the frust target guard and Cargo profile locks, removes eligible payloads and preserves
the target/profile directories, `deps` and `.fingerprint` roots, and Cargo lock files. Unknown or user-managed layouts
and fresh activity prevent retirement. Expired incremental reuse data is pruned in bounded batches first; a held
compiler session keeps its own state and prevents program-output retirement, while other expired reuse data can
still be reclaimed. Foreground fallback samples a bounded set of candidates; the daily
background collector drains eligible targets, and `FRUST_GC_SYNC=on` requests that drain inline.

Cleanup runs when frust is invoked; it is not a background service. Retention periods and soft budgets do not
impose a universal hard disk cap. See [incremental data and compaction](how-it-works.md#incremental-data-and-compaction)
for idle-state protection, cleanup retries and the effect of cache coverage.

The update check runs only on an interactive terminal, never when `CI` is set or `FRUST_BYPASS` is set, and at most
once a day. See [how it works](how-it-works.md#updates) for `frust self-update`.


`FRUST_LINK=smart fcargo build` enables the native-link policy. It applies when Cargo invokes rustc for an
unoptimised incremental binary; otherwise fresh artifacts remain fresh. The policy also works with
`FRUST_CACHE=off`, respects an existing foreign `RUSTC_WRAPPER`, and uses the current `xcrun` linker selection
rather than persisting an Xcode selection. `FRUST_DEBUG=1` reports applied and skipped decisions.

Measure actual function-body edits with `frust bench --linking --runs 7 --json`. The benchmark keeps both
incremental targets warm, alternates order, verifies that the binary recompiled and ran with the changed output,
and records native-link and total-build timing separately. It uses the same wrapper path in both arms to avoid
invalidating Cargo’s compiler-information cache between samples.

On Linux, `--linking` profiles only `FRUST_LINK=off` with the configured linker. It verifies the same real edits,
compiled-unit counts and changed runtime output, without reporting a smart-mode comparison. JSON reports
`runtime_outputs_equal: null` when there is only one mode. Windows profiling is not yet supported.
