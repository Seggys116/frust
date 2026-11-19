#!/usr/bin/env python3
"""Write the frozen runtime corpus.

The destination is an argument. The default is the managed build directory
target/compiler-rewrite/corpus. The script is deterministic: the same destination
bytes are produced on every run. It does not time any compiler.
"""

import argparse
import hashlib
import pathlib
import struct
import zlib

NEEDLE = b"frust_needle_7"
TEXT_BYTES = 32 * 1024 * 1024
RUST_FILES = 200
FUNCS_PER_FILE = 40
PNG_COUNT = 8
PNG_SIDE = 256


def chunk(tag: bytes, data: bytes) -> bytes:
    crc = zlib.crc32(tag + data) & 0xFFFFFFFF
    return struct.pack(">I", len(data)) + tag + data + struct.pack(">I", crc)


def write_png(path: pathlib.Path, index: int) -> None:
    raw = bytearray()
    for y in range(PNG_SIDE):
        raw.append(0)
        for x in range(PNG_SIDE):
            raw.append((x * (index + 3) + y) & 255)
            raw.append((y * (index + 5) + x * 2) & 255)
            raw.append(((x ^ y) * (index + 1)) & 255)
    ihdr = struct.pack(">IIBBBBB", PNG_SIDE, PNG_SIDE, 8, 2, 0, 0, 0)
    image = b"\x89PNG\r\n\x1a\n" + chunk(b"IHDR", ihdr)
    image += chunk(b"IDAT", zlib.compress(bytes(raw), 1))
    image += chunk(b"IEND", b"")
    path.write_bytes(image)


def write_text(path: pathlib.Path) -> None:
    line = bytearray()
    with path.open("wb") as handle:
        written = 0
        n = 0
        while written < TEXT_BYTES:
            line.clear()
            if n % 100 == 0:
                line += NEEDLE
                line += b" "
            line += b"row "
            line += str(n).encode("ascii")
            line += b" alpha beta gamma delta epsilon zeta eta theta\n"
            if written + len(line) > TEXT_BYTES:
                line = line[: TEXT_BYTES - written]
            handle.write(line)
            written += len(line)
            n += 1


def write_rust(directory: pathlib.Path) -> None:
    directory.mkdir(parents=True, exist_ok=True)
    (directory / "rustfmt.toml").write_text("edition = \"2021\"\n", encoding="utf-8")
    for file_index in range(RUST_FILES):
        parts = [f"// corpus file {file_index}\n"]
        for func_index in range(FUNCS_PER_FILE):
            name = f"item_{file_index}_{func_index}"
            parts.append(
                f"fn {name}(value: u64) -> u64 {{\n"
                f"    let mut total = value.wrapping_add({file_index + func_index});\n"
                f"    total = total.wrapping_mul(3);\n"
                f"    if total & 1 == 0 {{\n"
                f"        total = total.wrapping_add({func_index});\n"
                f"    }}\n"
                f"    total\n"
                f"}}\n"
            )
        (directory / f"item_{file_index:03d}.rs").write_text("".join(parts), encoding="utf-8")


def manifest(root: pathlib.Path) -> str:
    rows = []
    for path in sorted(p for p in root.rglob("*") if p.is_file()):
        digest = hashlib.sha256(path.read_bytes()).hexdigest()
        rel = path.relative_to(root).as_posix()
        rows.append(f"{digest}  {rel}\n")
    return "".join(rows)


def main() -> None:
    parser = argparse.ArgumentParser()
    parser.add_argument("dest", type=pathlib.Path)
    parser.add_argument("--manifest", type=pathlib.Path)
    args = parser.parse_args()
    dest = args.dest
    if dest.exists():
        raise SystemExit(f"destination already exists: {dest}")
    (dest / "pngs").mkdir(parents=True)
    write_text(dest / "search.txt")
    for index in range(PNG_COUNT):
        write_png(dest / "pngs" / f"image_{index}.png", index)
    write_rust(dest / "rust")
    text = manifest(dest)
    digest = hashlib.sha256(text.encode("utf-8")).hexdigest()
    print(digest)
    if args.manifest is not None:
        args.manifest.write_text(text, encoding="utf-8")


if __name__ == "__main__":
    main()
