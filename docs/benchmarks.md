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

Measured 2026-10-31 on Intel(R) Core(TM) Ultra 7 255H, 16 cores, Linux 7.0.0-42-generic x86_64, ext4, rustc 1.97.1. Load average for the general build rows was 2.59 before and 2.26 after; for the profile matrix it was 2.24 before and 2.66 after. Results describe these generated workloads on this machine.

![Debug cold builds, warm restores, no-op builds and real warm-session edits](../.github/assets/results.svg)

| Debug build | Cargo ms [min, max] | frust ms [min, max] | Result |
|---|---:|---:|---:|
| Cold build | 3336 [2825, 3531] | 3595 [3024, 3677] | +259 ms |
| Warm restore | 3483 [2863, 3567] | 161 [160, 168] | 21.6x faster |
| No-op build | 18 [17, 18] | 20 [18, 21] | +2 ms |
| First edit after restore | 89 [79, 104] | 113 [91, 151] | +24 ms |
| Warm-session edit | 91 [84, 101] | 105 [95, 139] | +14 ms |

The uncached, no-op and edit rows expose overhead alongside the cache gains.

![Release warm-cache rebuilds for default release, ThinLTO and full/fat LTO](../.github/assets/release-profiles.svg)

| Release profile | Build | Cargo ms [min, max] | frust ms [min, max] | Result |
|---|---|---:|---:|---:|
| Release defaults | Cold build | 3629 [3552, 3730] | 3853 [3758, 3937] | +224 ms |
| Release defaults | Warm restore | 3671 [3614, 3711] | 146 [144, 148] | 25.1x faster |
| Release defaults | No-op | 18 [17, 20] | 20 [18, 21] | +2 ms |
| Release defaults | Body edit | 67 [65, 71] | 71 [69, 79] | +4 ms |
| ThinLTO | Cold build | 5022 [4976, 5092] | 5199 [5140, 5291] | +177 ms |
| ThinLTO | Warm restore | 5049 [4893, 5082] | 149 [145, 166] | 33.9x faster |
| ThinLTO | No-op | 19 [18, 20] | 20 [19, 21] | +1 ms |
| ThinLTO | Body edit | 1758 [1715, 1827] | 1819 [1741, 1929] | +61 ms |
| Full / fat LTO | Cold build | 4989 [4959, 5100] | 5128 [5034, 5164] | +139 ms |
| Full / fat LTO | Warm restore | 4920 [4865, 5052] | 145 [141, 177] | 33.9x faster |
| Full / fat LTO | No-op | 18 [17, 19] | 20 [19, 21] | +2 ms |
| Full / fat LTO | Body edit | 1703 [1629, 1786] | 1701 [1664, 1773] | -2 ms |

Release defaults use `lto=false`, 16 codegen units and unwinding; ThinLTO uses `lto="thin"` with the same codegen/panic settings; full/fat LTO uses `lto="fat"`, one codegen unit and `panic="abort"`. Cargo’s `lto=false` permits thin local LTO; it is not the same setting as `lto="off"`.

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

The configured-linker baseline took 35 [30, 46] ms within the 87 [83, 97] ms build. Each of the 7 real body edits compiled exactly one current main binary and executed the expected changed output. The configured linker was retained.
