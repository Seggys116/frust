# Benchmark methodology

The compiler rewrite has no benchmark results yet. Timings from earlier versions of fRust describe a different toolchain architecture.

Compare fRust with the same upstream Rust and Cargo revision, under the same optimization, debug, panic, target, codegen-unit and linker settings. Include an upstream release baseline with incremental compilation and full LTO explicitly enabled whenever fRust uses those settings. Record the actual compiler and linker invocations.

Report clean builds with cold artifacts, Cargo no-op builds, local implementation edits, wider dependency changes and native relinks separately. For source edits, verify the edited target ran and log which phases did fresh work. Verify the edited result against a fresh build before counting reuse as a win. Record elapsed time, tail latency, peak memory and disk use on the stated host.

Measure runtime and executable size independently. Compare equivalent stripped artifacts and record debug-symbol size separately. Publish the command, inputs, toolchain versions, host details, seed state and repeat measurements for any public performance claim.
