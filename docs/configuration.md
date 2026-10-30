# Configuration

| Variable | Default | Meaning |
|---|---|---|
| `FRUST_HOME` | platform cache dir + `/frust` | All state lives here |
| `FRUST_CACHE` | `on` | Compile cache |
| `FRUST_CACHE_PRIMARY` | `on` | Cache workspace packages too (non-incremental builds only) |
| `FRUST_CACHE_PRIMARY_INCREMENTAL` | `on` | Also cache incrementally compiled workspace members you are building (clean rebuilds and branch switches restore them; edits cost about 12% more inside cargo) |
| `FRUST_COMPACT_INCREMENTAL_IDLE_DAYS` | `7` | A crate's incremental data is removed after this long untouched, when the cache holds its outputs |
| `FRUST_BENCH_MAX_SCRATCH_MIB` | `4096` | Stops `frust bench` when its scratch directory passes this physical size |
| `FRUST_CACHE_INCREMENTAL` | `on` | Cache incremental compiles of non-primary path dependencies in dev builds |
| `FRUST_CACHE_MAX_GIB` | `16` | Budget enforced by `frust gc` |
| `FRUST_AUTO_COMPACT` | `on` | Compact target directories automatically after successful builds |
| `FRUST_COMPACT_KEEP_DAYS` | `14` | How long an unused profile's artifacts are kept before auto-compaction removes them |
| `FRUST_TARGET_MAX_GIB` | `8` | Soft per-target-directory budget; over it, least recently used superseded artifacts go first |
| `FRUST_TARGET` | `central` | `local` leaves `CARGO_TARGET_DIR` alone |
| `FRUST_TUNE` | `off` | Opt-in dependency debuginfo reduction |
| `FRUST_LOCK` | `on` | Queued build lock |
| `FRUST_LOCK_TIMEOUT_SECS` | none | Give up waiting for the lock |
| `FRUST_RENDER` | `auto` | `auto`, `ansi`, `plain`, `off` |
| `FRUST_OSC` | `auto` | `auto`, `off`, `all`, or a list |
| `FRUST_CARGO`, `FRUST_RUSTC` | PATH lookup | Use a specific binary |
| `FRUST_BYPASS` | unset | Run cargo completely untouched |

An explicit `CARGO_TARGET_DIR`, `--target-dir`, or a `target-dir` in your cargo config is always respected.
