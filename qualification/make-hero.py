#!/usr/bin/env python3
"""Write .github/assets/hero.svg: the fRust wordmark and tagline. Published numbers live in the
benchmark results, not in this graphic."""
from pathlib import Path

GLYPHS = {
    "f": ["..##", ".#..", "###.", ".#..", ".#..", ".#..", ".#.."],
    "R": ["####.", "#...#", "#...#", "####.", "#.#..", "#..#.", "#...#"],
    "u": [None, None, "#...#", "#...#", "#...#", "#...#", ".###."],
    "s": [None, None, ".####", "#....", ".###.", "....#", "####."],
    "t": [None, ".#...", "####.", ".#...", ".#...", ".#..#", "..##."],
}
CELL = 11
FACE, SHADOW = "#ce422b", "#6e1f12"


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


def main():
    svg = f'''<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 960 190" width="960" height="190" role="img" aria-labelledby="title desc">
  <title id="title">fRust</title>
  <desc id="desc">fRust, a Rust toolchain with a Cargo that reuses verified work.</desc>
  <rect x="1" y="1" width="958" height="188" rx="17" fill="#0d1117" stroke="#30363d" stroke-width="2"/>
  {wordmark()}
  <text x="480" y="148" text-anchor="middle" font-family="ui-monospace,SFMono-Regular,Menlo,Consolas,monospace" font-size="15" fill="#8b949e">dependency cache / central targets / cooperative checks / managed cleanup</text>
</svg>
'''
    Path(__file__).resolve().parents[1].joinpath(".github/assets/hero.svg").write_text(svg)


if __name__ == "__main__":
    main()
