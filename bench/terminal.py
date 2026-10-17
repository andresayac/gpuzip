"""Draws a terminal session as SVG, from its real output with colors, for the README.

  python bench/terminal.py <transcript> <out.svg> [--title TEXT] [--cols 100]

<transcript> is what the commands printed with colors forced on (CLICOLOR_FORCE=1), each command
on a line of its own starting with "$ ". Lines longer than --cols are cut with an ellipsis.
Only the SGR codes gpuzip uses are read: reset, bold, dim, and the 8 + 8 foreground colors.
"""

import argparse
import re
from html import escape

FONT = "ui-monospace, 'Cascadia Mono', 'SF Mono', Menlo, Consolas, 'DejaVu Sans Mono', monospace"
BG, FG, DIM_FG, PROMPT = "#161b22", "#e6edf3", "#7d8590", "#7ee787"
COLORS = {30: "#484f58", 31: "#ff7b72", 32: "#3fb950", 33: "#d29922", 34: "#58a6ff", 35: "#bc8cff", 36: "#39c5cf", 37: "#b1bac4"}
COLORS.update({k + 60: v for k, v in COLORS.items()})
SGR = re.compile(r"\x1b\[([0-9;]*)m")
BLOCKS = set("█▓▒░")  # the glyphs of gpuzip's bars, each part in its own color


def spans(line):
    """(text, color, bold, dim) runs of one line"""
    out, color, bold, dim, pos = [], None, False, False, 0
    for m in SGR.finditer(line):
        if m.start() > pos:
            out.append((line[pos:m.start()], color, bold, dim))
        for code in [int(c) for c in (m.group(1) or "0").split(";") if c]:
            if code == 0:
                color, bold, dim = None, False, False
            elif code == 1:
                bold = True
            elif code == 2:
                dim = True
            elif code == 22:
                bold = dim = False
            elif code == 39:
                color = None
            elif code in COLORS:
                color = COLORS[code]
        pos = m.end()
    if pos < len(line):
        out.append((line[pos:], color, bold, dim))
    return out


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("transcript")
    ap.add_argument("out")
    ap.add_argument("--title", default="")
    ap.add_argument("--cols", type=int, default=100)
    a = ap.parse_args()
    lines = open(a.transcript, encoding="utf-8").read().rstrip("\n").split("\n")
    cw, lh, pad, top = 8.4, 20, 18, 44
    W = int(pad * 2 + cw * a.cols)
    H = top + lh * len(lines) + pad
    parts = [f'<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 {W} {H}" width="{W}" height="{H}" '
             f'font-family="{FONT}" font-size="14" role="img" aria-label="{escape(a.title or "terminal session")}">',
             f'<rect width="{W}" height="{H}" rx="10" fill="{BG}"/>']
    for i, c in enumerate(("#ff5f57", "#febc2e", "#28c840")):
        parts.append(f'<circle cx="{pad + 6 + i * 20}" cy="18" r="6" fill="{c}"/>')
    if a.title:
        parts.append(f'<text x="{W / 2}" y="23" fill="{DIM_FG}" font-size="13" text-anchor="middle">{escape(a.title)}</text>')
    for n, line in enumerate(lines):
        y = top + lh * n + 14
        runs = spans(line)
        if line.startswith("$ "):
            runs = [("$ ", PROMPT, True, False), (SGR.sub("", line[2:]), FG, True, False)]
        x, used, tspans = pad, 0, []
        for text, color, bold, dim in runs:
            if used >= a.cols:
                break
            if used + len(text) > a.cols:
                text = text[: a.cols - used - 1] + "…"
            fill = DIM_FG if dim else (color or FG)
            if text and set(text) <= BLOCKS:  # a bar: drawn as a rectangle, crisper than block glyphs
                parts.append(f'<rect x="{x + used * cw:.1f}" y="{y - 12}" width="{len(text) * cw:.1f}" height="14" fill="{fill}"/>')
            else:
                weight = ' font-weight="700"' if bold else ""
                tspans.append(f'<tspan x="{x + used * cw:.1f}" fill="{fill}"{weight}>{escape(text)}</tspan>')
            used += len(text)
        parts.append(f'<text y="{y}" xml:space="preserve">{"".join(tspans)}</text>')
    parts.append("</svg>\n")
    open(a.out, "w", encoding="utf-8", newline="\n").write("".join(parts))
    print(a.out)


if __name__ == "__main__":
    main()
