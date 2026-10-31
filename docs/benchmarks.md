# Benchmarks

## Method

The benchmark compares plain Cargo with release-built frust binaries using generated projects and isolated temporary caches and targets. It runs offline and alternates which tool goes first. Cold means an empty target and frust cache; it does not mean the operating system page cache was cleared.

Every reported build-timing row has 7 samples after untimed tool/profile warm-ups. Tables show median [minimum, maximum] milliseconds; figure whiskers show the same range, not confidence intervals. Ratios divide the Cargo median by the frust median. Display and inline maintenance are off in the speed rows; synchronous cache storage is included.

The debug fixture uses 4 local crates plus serde 1.0.229 and serde_json 1.0.154. The release-profile fixture uses 2 local crates plus serde 1.0.229 and serde_json 1.0.154. Edits change an observable main-function body value. The first edit after restoration includes rebuilding incremental state; the warm-session edit measures the next real edit.

A stable compiler audit records actual rustc compilation calls, excluding version and `--print` queries, separately from Cargo’s `Compiling` package notices and cache hit/miss events. Warm restores must execute zero declared Cargo-unit compilations and zero unknown compiler calls. Build-script capability probes remain explicitly counted. Build wall times include audit shim startup and logging. Every no-op and final unchanged growth rebuild must emit zero Cargo compile notices, execute zero compiler calls in every classification, and record zero cache hits or misses. Every release-profile build runs its executable, verifies the changed output, and checks Cargo/frust byte identity at the same source and target paths.

The growth experiment replays 200 body/signature edits, feature changes and reverts across 12 local crates plus serde and serde_json. This workload assesses automatic compaction after every build, rather than the ordinary ten-minute interval, with a 2-second / 512-MiB limit on deduplication and compression work and fixed retention policy. Structural analysis and guarded cleanup are additional work. Storage includes the target and cache, with shared extents counted once; the toolchain and shared Cargo registry are excluded. Growth-loop timings are not a latency comparison.

The link collector measures the configured-linker baseline, with artifact caching off and only the primary application profiled using benchmark-only `RUSTC_BOOTSTRAP=1` and `-Ztime-passes`. Instrumentation and system load can affect the timings.

Reproduce with release-built binaries and at least 12 GiB free. Set `TMPDIR` to a scratch directory on the filesystem being measured; these results use ext4 rather than the system tmpfs:

```sh
export TMPDIR=/path/to/scratch-on-measured-filesystem
frust bench --quick --runs 7 --json
frust bench --growth --edits=200 --every=20 --libs=12 --variants=cargo,auto --json
frust bench --growth --edits=200 --every=20 --libs=12 --variants=cargo,auto --compression=off --json
frust bench --linking --quick --runs 7 --json
```

## Results

Measured 2026-10-31 on Intel(R) Core(TM) Ultra 7 255H, 16 cores, Linux 7.0.0-42-generic x86_64, ext4, rustc 1.97.1. Load average for the general build rows was 3.25 before and 3.04 after; for the profile matrix it was 3.96 before and 2.61 after. Results describe these generated workloads on this machine.

![Debug cold builds, warm restores, no-op builds and real warm-session edits](../.github/assets/results.svg)

| Debug build | Cargo ms [min, max] | frust ms [min, max] | Result |
|---|---:|---:|---:|
| Cold build | 4773 [3156, 5123] | 5184 [3373, 5275] | +411 ms |
| Warm restore | 5066 [3197, 5086] | 175 [170, 182] | 28.9x faster |
| No-op build | 20 [19, 21] | 21 [21, 23] | +1 ms |
| First edit after restore | 95 [89, 100] | 121 [107, 130] | +26 ms |
| Warm-session edit | 93 [86, 100] | 113 [99, 176] | +20 ms |

The uncached, no-op and edit rows expose overhead alongside the cache gains.

![Release warm-cache rebuilds for default release, ThinLTO and full/fat LTO](../.github/assets/release-profiles.svg)

| Release profile | Build | Cargo ms [min, max] | frust ms [min, max] | Result |
|---|---|---:|---:|---:|
| Release defaults | Cold build | 5071 [5042, 5093] | 5174 [5147, 5214] | +103 ms |
| Release defaults | Warm restore | 5086 [5044, 5117] | 161 [149, 180] | 31.6x faster |
| Release defaults | No-op | 20 [18, 21] | 21 [20, 22] | +1 ms |
| Release defaults | Body edit | 76 [67, 80] | 79 [73, 83] | +3 ms |
| ThinLTO | Cold build | 7263 [6963, 7496] | 7296 [7244, 7708] | +33 ms |
| ThinLTO | Warm restore | 7181 [7092, 7397] | 159 [156, 165] | 45.2x faster |
| ThinLTO | No-op | 20 [19, 22] | 22 [21, 23] | +2 ms |
| ThinLTO | Body edit | 2407 [2364, 2519] | 2389 [2360, 2583] | -18 ms |
| Full / fat LTO | Cold build | 6832 [6590, 7388] | 7093 [6760, 7436] | +261 ms |
| Full / fat LTO | Warm restore | 6725 [6524, 7379] | 158 [154, 168] | 42.6x faster |
| Full / fat LTO | No-op | 19 [19, 22] | 21 [20, 23] | +2 ms |
| Full / fat LTO | Body edit | 2216 [2136, 2279] | 2204 [2131, 2217] | -12 ms |

Release defaults use `lto=false`, 16 codegen units and unwinding; ThinLTO uses `lto="thin"` with the same codegen/panic settings; full/fat LTO uses `lto="fat"`, one codegen unit and `panic="abort"`. Cargo’s `lto=false` permits thin local LTO; it is not the same setting as `lto="off"`.

Median paired cold-build differences (frust minus Cargo) were Release defaults +114 ms (7/7 positive pairs); ThinLTO +307 ms (6/7 positive pairs); Full / fat LTO +184 ms (5/7 positive pairs). These are medians of paired differences; the cold-build table result cells subtract the separately computed tool medians.

All paired release executables were byte-identical, including cold versus restored outputs. Each warm restore executed zero declared Cargo-unit or unknown compiler calls and 1 build-script capability probe. The separate release runtime check also matched output on every one of its 10 measured launches per binary; process-launch timing is not a program-throughput result.

![Combined target and cache storage after 200 edits](../.github/assets/growth.svg)

| After 200 edits | Target MiB | Cache MiB | Combined physical MiB |
|---|---:|---:|---:|
| Cargo | 371.1 | 0.0 | 371.1 |
| frust compression off | 174.4 | 164.1 | 338.5 |
| frust compression auto | 174.4 | 64.5 | 238.8 |

With automatic cache compression, combined storage was 35.6% smaller than Cargo. Both variants executed 1,542 actual compiler invocations during the automatic-compression edit loop and executed zero compiler calls on the final unchanged rebuild, with zero Cargo compile notices or cache events; every edit’s runtime output matched. Compression-off and compression-auto are separate full replays of the same workload.

Automatic compression used 60.7% less cache allocation and 29.4% less combined allocation than disabled compression.

After the initial two-project debug builds, combined allocation was 221.1 MiB for Cargo and 260.2 MiB for frust including its cache: 17.7% larger. Cache storage has an initial cost; total savings depend on the workload and accumulated build history.

The configured-linker baseline took 36 [30, 42] ms within the 92 [82, 99] ms build. Each of the 7 real body edits compiled exactly one current main binary and executed the expected changed output. The configured linker was retained.
