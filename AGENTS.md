# fRust repository instructions

## Scope

- fRust is a Rust compiler and Cargo toolchain for macOS and Linux. Use a matching compiler, standard library, Cargo and rustdoc built from pinned upstream sources. Keep normal Cargo and rustc entry points, flags, output conventions, cross-compilation behavior and explicit tool selections working.
- Keep compatibility, invalidation correctness and generated-code quality observable. Full LTO must run when requested and its whole-program inputs require it. When a reuse decision cannot be justified, use the normal compilation or link path in the fRust toolchain.
- Keep the README, public docs, changelog and relevant checks in sync with delivered behavior. Date and qualify any support or benchmark claim with the evidence that establishes it.

## Code quality

- Use clean, readable code and a clear structure. Match upstream conventions in upstream code and keep patches focused. Preserve license notices, required documentation and upstream comments.
- Target 1–5% comment density per project-owned handwritten source file. Divide physical comment lines by nonblank physical lines. Use short comments for intent or invariants and avoid paragraph comments. For files too short to fit one line in the range, use judgment and avoid filler.
- Profile the hot path. Pay attention to filesystem traffic, serialization, allocation, memory use and job scheduling as well as CPU time. Keep normal compilation viable when caches are empty, unavailable or discarded.
- Show that edited and fresh builds agree before enabling reuse. Use matched upstream comparisons and measure build latency, output performance and artifact size separately. Include native Linux and macOS verification before claiming support.

## Working tree and commits

- Keep changes in ordinary scoped local commits. Leave out co-authoring and AI attribution. Commit this `AGENTS.md` and public project documentation. Keep `goal.md`, `plan.md`, `plans.md`, `research.md` and `.research/` local and ignored, including raw private benchmark data.
- Treat sibling projects as read-only. Use isolated source snapshots for private benchmarks and the configured repository build directory for outputs. Keep private source and results local, and remove temporary snapshots when finished.
- Publish or push only with explicit authorization. Distribute the toolchain as platform packages when release work is authorized.
