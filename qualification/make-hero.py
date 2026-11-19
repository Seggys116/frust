#!/usr/bin/env python3
"""Write .github/assets/hero.svg from the measured ratios in qualification/OPTIMIZED-LINUX.md.

Each bar is fRust CPU time as a fraction of official Rust 1.100.0 on the same pinned cores; the
dim track is official Rust (1.00). Update ROWS from the notes when the measurements change.
"""
from pathlib import Path

ROWS = [
    ("clean rebuild, fd", 0.024),
    ("clean rebuild, ripgrep", 0.331),
    ("no-op build", 0.90),
    ("first build, empty cache", 1.02),
]
FOOTER = "Linux x86-64 / 19 Nov 2026 / CPU time on pinned cores, median of 3 rounds / official Rust 1.100.0 = 1.00"

GLYPHS = {
    "f": ["..##", ".#..", "###.", ".#..", ".#..", ".#..", ".#.."],
    "R": ["####.", "#...#", "#...#", "####.", "#.#..", "#..#.", "#...#"],
    "u": [None, None, "#...#", "#...#", "#...#", "#...#", ".###."],
    "s": [None, None, ".####", "#....", ".###.", "....#", "####."],
    "t": [None, ".#...", "####.", ".#...", ".#...", ".#..#", "..##."],
}
CELL = 11
FACE, SHADOW = "#ce422b", "#6e1f12"
SEGMENTS, PITCH, TRACK_X = 36, 15, 266
MAX_SEGMENTS = 38


def wordmark():
    columns = sum(len(next(row for row in GLYPHS[ch] if row)) for ch in "fRust") + 4
    x0 = (960 - columns * CELL) // 2
    y0 = 28
    shadow, face = [], []
    x = x0
    for ch in "fRust":
        rows = GLYPHS[ch]
        width = len(next(row for row in rows if row))
        for r, row in enumerate(rows):
            for c, mark in enumerate(row or ""):
                if mark == "#":
                    cell = f'<rect x="{x + c * CELL}" y="{y0 + r * CELL}" width="{CELL}" height="{CELL}"/>'
                    face.append(cell)
                    shadow.append(cell.replace(f'x="{x + c * CELL}"', f'x="{x + c * CELL + 3}"')
                                  .replace(f'y="{y0 + r * CELL}"', f'y="{y0 + r * CELL + 3}"'))
        x += (width + 1) * CELL
    return (f'<g shape-rendering="crispEdges"><g fill="{SHADOW}">{"".join(shadow)}</g>'
            f'<g fill="{FACE}">{"".join(face)}</g></g>')


def bars():
    out = []
    for index, (label, ratio) in enumerate(ROWS):
        y = 188 + index * 22
        track = "".join(f'<rect x="{TRACK_X + i * PITCH}" y="{y}" width="12" height="9" fill="#21262d"/>'
                        for i in range(SEGMENTS))
        filled = min(MAX_SEGMENTS, max(1, round(ratio * SEGMENTS)))
        fill = "".join(
            f'<rect x="{TRACK_X + i * PITCH}" y="{y}" width="12" height="9" '
            f'fill="{FACE if i < SEGMENTS else "#d29922"}"/>' for i in range(filled))
        out.append(f'<text x="{TRACK_X - 14}" y="{y + 9}" text-anchor="end" font-size="12" fill="#8b949e">{label}</text>')
        out.append(f'<g shape-rendering="crispEdges">{track}{fill}</g>')
        out.append(f'<text x="{TRACK_X + MAX_SEGMENTS * PITCH + 8}" y="{y + 9}" font-size="12" fill="#c9d1d9">{ratio:.2f}x</text>')
    return "\n  ".join(out)


def main():
    summary = "; ".join(f"{label} {ratio:.2f}x" for label, ratio in ROWS)
    svg = f'''<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 960 300" width="960" height="300" role="img" aria-labelledby="title desc">
  <title id="title">fRust</title>
  <desc id="desc">fRust CPU time as a fraction of official Rust 1.100.0 on Linux x86-64 (19 November 2026, pinned cores, median of three rounds): {summary}.</desc>
  <rect x="1" y="1" width="958" height="298" rx="17" fill="#0d1117" stroke="#30363d" stroke-width="2"/>
  {wordmark()}
  <g font-family="ui-monospace,SFMono-Regular,Menlo,Consolas,monospace">
  <text x="480" y="143" text-anchor="middle" font-size="15" fill="#8b949e">dependency cache / central targets / cooperative checks / managed cleanup</text>
  <text x="480" y="170" text-anchor="middle" font-size="11" letter-spacing="1.5" fill="#6e7681">CPU TIME VS OFFICIAL RUST, LOWER IS BETTER</text>
  {bars()}
  <text x="480" y="284" text-anchor="middle" font-size="10" fill="#6e7681">{FOOTER}</text>
  </g>
</svg>
'''
    Path(__file__).resolve().parents[1].joinpath(".github/assets/hero.svg").write_text(svg)


if __name__ == "__main__":
    main()
