# Runtime corpus

Frozen before any candidate runtime sample. The bytes are produced by `qualification/make_corpus.py` and checked by `qualification/corpus.sha256`. That manifest's sha256 is `7fee391809c5463bd1b9b6e625a2ecbe61bfd3c214c5830cc7e2ee256544f000`. It lists 210 files.

Regenerate into an empty directory and compare:

```
python3 qualification/make_corpus.py DEST --manifest NEW_MANIFEST
cmp NEW_MANIFEST qualification/corpus.sha256
```

The copy used for measurement lives under the managed build directory `target/compiler-rewrite/corpus`. Timed outputs go outside that directory.

The corpus was frozen before any runtime sample. Runtime samples taken with it are in `qualification/CANDIDATE.md`, on Linux x86-64 on 2026-11-19. They cover the ripgrep command and the oxipng command only. The rustfmt command has not been timed, because locked rustfmt 1.9.0 does not build with the compilers tried. Those samples are single sessions, not a passing gate, and the control ripgrep median moved between sessions from 0.0048 s to about 0.0062 s.

## Commands

ripgrep, one file, so the tool's own path already forces one thread. The flag is still set:

```
rg --no-config -j 1 -F frust_needle_7 search.txt
```

rustfmt, on a copy of `rust/` so the frozen files stay unchanged. `rustfmt.toml` in that tree sets edition 2021:

```
rustfmt --edition 2021 item_*.rs
```

oxipng, preset 4 from its own help (`--zc 12`, filters `0,1,6,7,8,9`, brute level 1). Threads are pinned to 1 so the sample does not follow the host's CPU count. Output goes to a separate directory:

```
oxipng -o 4 --threads 1 --dir OUT pngs/*.png
```

Level 4 was chosen before any runtime sample because the default level 2 is described as quite fast, and the 1% runtime bound needs a run long enough to measure. The preset itself is the upstream one.
