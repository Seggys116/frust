#!/usr/bin/env python3
"""Report how each toolchain's compiler libraries are mapped during a compile.

For every PREFIX, compiles a small generated library with PREFIX/bin/rustc and samples
/proc/PID/smaps of the compiler while it runs. Prints the largest Rss and FilePmdMapped seen for
the librustc_driver and libLLVM mappings. FilePmdMapped > 0 means the page cache holds that text in
2 MiB folios and the compiler maps it with 2 MiB pages; 0 means 4 KiB pages.

Whether that happens depends on how the file entered the page cache, not on its bytes: buffered
writes of 2 MiB or more (rustup writes 16 MiB, frust-install rewrites large files with 32 MiB
writes) give large folios on Linux file systems with large folio support (ext4 from 6.16, XFS);
tar's 10 KiB writes, cp (copy_file_range), and faulting an evicted file back in through ld.so give
small ones. Run this before timing toolchains against each other and record the output: a
toolchain at 0 against one at 30-50 MiB is about 1 % slower on an Intel Core Ultra for that reason
alone. The compile itself only faults pages, as any compile does.

  toolchain-folio-state.py PREFIX...      (Linux only)
"""
import os
from pathlib import Path
import subprocess
import sys
import tempfile
import time

SOURCE = "\n".join(
    f"pub fn f{i}(v: &[u64]) -> u64 {{ v.iter().map(|x| x.wrapping_mul({i} + 1) ^ (x >> 3))"
    f".filter(|x| x % 7 != {i % 7}).fold(0u64, |a, b| a.wrapping_add(b)) }}\n"
    f"pub fn g{i}(s: &str) -> Vec<String> {{ s.split(',').map(|p| format!(\"{{}}-{i}\", p.trim())).collect() }}"
    for i in range(200))


def sample(pid):
    totals = {}
    current = None
    try:
        with open(f"/proc/{pid}/smaps") as handle:
            for line in handle:
                head = line.split()
                if head and not head[0].endswith(":"):
                    current = head[5] if len(head) > 5 else None
                    continue
                if current and ("librustc_driver" in current or "libLLVM" in current):
                    if head[0] in ("Rss:", "FilePmdMapped:"):
                        key = ("libLLVM" if "libLLVM" in current else "librustc_driver", head[0][:-1])
                        totals[key] = totals.get(key, 0) + int(head[1])
    except (FileNotFoundError, ProcessLookupError):
        return {}
    return totals


def probe(prefix, scratch):
    source = Path(scratch) / "folio_probe.rs"
    source.write_text(SOURCE)
    child = subprocess.Popen([str(Path(prefix) / "bin" / "rustc"), "--crate-type", "lib", "-C", "opt-level=2",
                              "--out-dir", scratch, str(source)], stderr=subprocess.DEVNULL)
    best = {}
    while child.poll() is None:
        for key, value in sample(child.pid).items():
            best[key] = max(best.get(key, 0), value)
        time.sleep(0.02)
    if child.returncode != 0:
        sys.exit(f"{prefix}: rustc exited {child.returncode}")
    return best


def main():
    if len(sys.argv) < 2 or not sys.platform.startswith("linux"):
        sys.exit(__doc__)
    with tempfile.TemporaryDirectory() as scratch:
        for prefix in sys.argv[1:]:
            best = probe(prefix, scratch)
            fields = " ".join(f"{lib}_{field}={best.get((lib, field), 0) >> 10}MiB"
                              for lib in ("librustc_driver", "libLLVM") for field in ("Rss", "FilePmdMapped"))
            print(f"{prefix} {fields}", flush=True)


if __name__ == "__main__":
    main()
