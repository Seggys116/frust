#!/usr/bin/env python3
"""Put the shared libraries of several toolchains into the same page-cache state before a CPU
benchmark.

Whether a process maps rustc_driver and libLLVM text with 2 MiB pages (FilePmdMapped in
/proc/PID/smaps) depends on the folios the file already has in the page cache, not on the file:
the official toolchain measured with 32 MiB of driver text PMD-mapped while a byte-identical
libLLVM in a fresh package had none. Comparing toolchains in different states measures iTLB
behaviour of the cache, not the compilers.

  evict  drop the clean cached pages of each file (POSIX_FADV_DONTNEED); the next run faults
         them back with ordinary small folios
  huge   evict, then fault every page back through a MADV_HUGEPAGE mapping so the cache holds
         PMD-sized folios wherever the file layout allows

Pages mapped by a running process are not dropped; `fincore` shows what is cached.
Usage: page-cache-state.py {evict|huge} PREFIX...
"""
import mmap
import os
from pathlib import Path
import sys


def files(prefix):
    lib = Path(prefix) / "lib"
    found = sorted(p for p in lib.glob("*.so*") if p.is_file() and not p.is_symlink())
    found += sorted(p for p in (lib / "rustlib").glob("*/lib/libstd-*.so") if p.is_file())
    found.append(Path(prefix) / "bin" / "rustc")
    return found


def main():
    if len(sys.argv) < 3 or sys.argv[1] not in ("evict", "huge"):
        sys.exit(__doc__)
    mode = sys.argv[1]
    for prefix in sys.argv[2:]:
        for path in files(prefix):
            fd = os.open(path, os.O_RDONLY)
            try:
                os.posix_fadvise(fd, 0, 0, os.POSIX_FADV_DONTNEED)
                size = os.fstat(fd).st_size
                if mode == "huge" and size:
                    view = mmap.mmap(fd, size, prot=mmap.PROT_READ)
                    view.madvise(mmap.MADV_HUGEPAGE)
                    total = 0
                    for offset in range(0, size, mmap.PAGESIZE):
                        total += view[offset]
                    view.close()
            finally:
                os.close(fd)
            print(mode, path)


if __name__ == "__main__":
    main()
