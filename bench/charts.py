"""The README's charts, drawn as SVG from the measured results in this folder (no dependencies).

  python bench/charts.py [--out docs/img]

Each chart is written twice, -light.svg and -dark.svg, for GitHub's <picture> switch:
  rescue      what comes back from damaged files, gpuzip rescue against the usual tools
              (results-rescue.json, from research/recovery/evaluate.py)
  rescue-map  one damaged tar.gz byte by byte: what gzip gives, what gpuzip rescue gives
              (results-rescue-map.json, from rescue_map.py)
  zip-speed   testing ZIP archives, 7-Zip against gpuzip on the CPU (results.json, bench.py)
  gzip-speed  testing a 6.4 GB .gz and a 1.4 GB tar.gz (results-gz.json, bench_gz.py)

One series is the point and the rest are context, so gpuzip is drawn in the accent and every
other tool in gray. Colors follow the data-viz reference palette, light and dark steps.
"""

import argparse
import json
import os
from html import escape

HERE = os.path.dirname(os.path.abspath(__file__))
FONT = "system-ui, -apple-system, 'Segoe UI', Helvetica, Arial, sans-serif"
THEMES = {
    "light": dict(surface="#fcfcfb", ink="#0b0b0b", ink2="#52514e", muted="#898781", grid="#e1e0d9",
                  base="#c3c2b7", accent="#2a78d6", other="#898781",
                  exact="#0ca30c", inferred="#2a78d6", suspect="#fab219", unknown="#d03b3b", lost="#e1e0d9"),
    "dark": dict(surface="#1a1a19", ink="#f0efec", ink2="#c3c2b7", muted="#898781", grid="#2c2c2a",
                 base="#383835", accent="#3987e5", other="#898781",
                 exact="#0ca30c", inferred="#3987e5", suspect="#fab219", unknown="#d03b3b", lost="#383835"),
}
FAILURES = [  # research/recovery/realistic.py, in its order
    ("ftp_ascii", "FTP in ASCII mode (LF to CR LF)"),
    ("ascii_strip", "FTP in ASCII mode (CR LF to LF)"),
    ("bad_sectors", "three bad disk sectors"),
    ("flash_ff", "a lost flash erase block"),
    ("crosslinked", "cross-linked disk clusters"),
    ("torrent_holes", "a torrent stalled at 97%"),
    ("cut", "a download cut short"),
    ("resume_dup", "a resumed download, 64 KB twice"),
    ("resume_gap", "a resumed download, 512 KB gap"),
    ("crash_tail", "a crash while copying"),
    ("bitrot", "bit rot, eight flipped bits"),
]


class Svg:
    def __init__(self, w, h, t, title, desc):
        self.w, self.h, self.t, self.parts = w, h, t, []
        self.head = (f'<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 {w} {h}" width="{w}" height="{h}" '
                     f'font-family="{FONT}" role="img" aria-labelledby="t d">'
                     f'<title id="t">{escape(title)}</title><desc id="d">{escape(desc)}</desc>'
                     f'<rect width="{w}" height="{h}" rx="8" fill="{t["surface"]}"/>')

    def text(self, x, y, s, size=12, fill="ink2", anchor="start", weight=400, extra=""):
        self.parts.append(f'<text x="{x:.1f}" y="{y:.1f}" font-size="{size}" fill="{self.t.get(fill, fill)}" '
                          f'text-anchor="{anchor}" font-weight="{weight}"{extra}>{escape(s)}</text>')

    def line(self, x1, y1, x2, y2, stroke="grid", width=1):
        self.parts.append(f'<line x1="{x1:.1f}" y1="{y1:.1f}" x2="{x2:.1f}" y2="{y2:.1f}" '
                          f'stroke="{self.t.get(stroke, stroke)}" stroke-width="{width}" stroke-linecap="round"/>')

    def dot(self, x, y, fill, tip, r=4.5):
        self.parts.append(f'<circle cx="{x:.1f}" cy="{y:.1f}" r="{r}" fill="{self.t[fill]}" '
                          f'stroke="{self.t["surface"]}" stroke-width="2"><title>{escape(tip)}</title></circle>')

    def bar(self, x, y, w, h, fill, tip):
        """A horizontal bar from x, its data end (right) rounded by 4px, square at the baseline"""
        w, r = max(w, 1.0), min(4, max(w, 1.0) / 2, h / 2)
        d = (f"M{x:.1f},{y:.1f} h{w - r:.1f} a{r},{r} 0 0 1 {r},{r} v{h - 2 * r:.1f} "
             f"a{r},{r} 0 0 1 -{r},{r} h-{w - r:.1f} z")
        self.parts.append(f'<path d="{d}" fill="{self.t[fill]}"><title>{escape(tip)}</title></path>')

    def rect(self, x, y, w, h, fill):
        self.parts.append(f'<rect x="{x:.2f}" y="{y:.2f}" width="{w:.2f}" height="{h:.2f}" fill="{self.t[fill]}"/>')

    def legend(self, x, y, items):
        """items: (kind, color, label); kind 'dot' or 'swatch'. Returns the x after the last item."""
        for kind, color, label in items:
            if kind == "dot":
                self.parts.append(f'<circle cx="{x + 5}" cy="{y - 4}" r="5" fill="{self.t[color]}"/>')
            else:
                self.parts.append(f'<rect x="{x}" y="{y - 9}" width="10" height="10" rx="2" fill="{self.t[color]}"/>')
            self.text(x + 16, y, label, 12, "ink2")
            x += 16 + 6.6 * len(label) + 22
        return x

    def save(self, path):
        with open(path, "w", encoding="utf-8", newline="\n") as f:
            f.write(self.head + "".join(self.parts) + "</svg>\n")


def header(s, title, subtitle):
    s.text(24, 34, title, 17, "ink", weight=600)
    s.text(24, 56, subtitle, 13, "ink2")


def rescue_chart(t):
    data = json.load(open(os.path.join(HERE, "results-rescue.json"), encoding="utf-8"))
    by = {(r["original"], r["failure"]): r for r in data}
    panels = [
        ("logs.txt.gz", "a 32 MB log.gz", "bytes right"),
        ("code.tar.gz", "a tar.gz of 1,828 source files", "files intact"),
        ("lib.zip", "a ZIP of 3,440 source files", "entries intact"),
    ]
    W, label_w, top, row = 900, 236, 140, 26
    gap, right = 30, 24
    pw = (W - label_w - right - gap * (len(panels) - 1)) / len(panels)
    H = top + row * len(FAILURES) + 64
    s = Svg(W, H, t, "What comes back from a damaged archive",
            "For eleven ways files really break, the share of the original recovered intact by the "
            "usual tools (gzip, tar, 7-Zip, Python zipfile, the best of them) and by gpuzip rescue.")
    header(s, "What comes back from a damaged archive",
           "Eleven ways files really break, three archives. Share of the original recovered intact.")
    s.legend(24, 86, [("dot", "other", "best of gzip, tar, 7-Zip, Python zipfile"), ("dot", "accent", "gpuzip rescue")])
    for i, (_, name) in enumerate(FAILURES):
        s.text(label_w - 14, top + row * i + row / 2 + 4, name, 12, "ink2", "end")
    for p, (orig, title, unit) in enumerate(panels):
        x0 = label_w + p * (pw + gap)
        s.text(x0, top - 30, title, 12, "ink", weight=600)
        s.text(x0, top - 14, unit, 11, "muted")
        y_end = top + row * len(FAILURES)
        for k, anchor in ((0, "start"), (50, "middle"), (100, "end")):
            x = x0 + pw * k / 100
            s.line(x, top - 6, x, y_end, "grid")
            s.text(x, y_end + 16, f"{k}%", 11, "muted", anchor)
        for i, (failure, name) in enumerate(FAILURES):
            r = by[(orig, failure)]
            of = r.get("of")
            pct = (lambda v: v) if of is None else (lambda v: 100 * v / of)
            other, ours = pct(max(r["baseline"].values())), pct(r["gpuzip"])
            y = top + row * i + row / 2
            xo, xg = x0 + pw * other / 100, x0 + pw * ours / 100
            if xg - xo > 9:
                s.line(xo + 5, y, xg - 5, y, "base", 2)
            fmt = (lambda v: f"{v:.1f}%") if of is None else (lambda v: f"{v:,.0f} of {of:,}")
            s.dot(xo, y, "other", f"{name}, {title}: best other tool {fmt(r['baseline'][max(r['baseline'], key=r['baseline'].get)])}")
            s.dot(xg, y, "accent", f"{name}, {title}: gpuzip rescue {fmt(r['gpuzip'])}")
    s.text(24, H - 20, "On the bit-rotted log, 9 MB that gpuzip marked exact were wrong; its report said the checksum "
           "could not be checked. Data: bench/results-rescue.json.", 11, "muted")
    return s


def rescue_map_chart(t):
    m = json.load(open(os.path.join(HERE, "results-rescue-map.json"), encoding="utf-8"))
    W, x0, x1 = 900, 150, 876
    H = 230
    s = Svg(W, H, t, "One damaged tar.gz, byte by byte",
            f"{m['label']}: gzip gives the first {100 * m['gzip_prefix'] / m['output_size']:.1f}% of the data right; "
            f"gpuzip rescue gives {100 * m['totals']['exact'] / m['output_size']:.1f}% exact and marks the rest.")
    header(s, "One damaged tar.gz, byte by byte", m["label"])
    size, bins = m["output_size"], m["bins"]
    bw = (x1 - x0) / len(bins)
    order = ["exact", "inferred", "suspect", "unknown"]

    def strip(y, h, title, sub, fill_bin):
        s.text(x0 - 14, y + h / 2 - 1, title, 12, "ink", "end", 600)
        s.text(x0 - 14, y + h / 2 + 14, sub, 11, "muted", "end")
        for i, b in enumerate(bins):
            yy = y
            for k, frac in fill_bin(i, b):
                if frac > 0:
                    s.rect(x0 + i * bw, yy, bw + 0.35, h * frac, k)
                    yy += h * frac

    prefix = m["gzip_prefix"]
    split = x0 + (x1 - x0) * prefix / size  # gzip: right up to the first damage, nothing after
    s.text(x0 - 14, 96 + 17 - 1, "gzip, tar", 12, "ink", "end", 600)
    s.text(x0 - 14, 96 + 17 + 14, f"{100 * prefix / size:.1f}% right", 11, "muted", "end")
    s.rect(x0, 96, split - x0, 34, "exact")
    s.rect(split, 96, x1 - split, 34, "lost")
    strip(150, 34, "gpuzip rescue", f"{100 * m['totals']['exact'] / size:.1f}% exact",
          lambda i, b: [(k, b.get(k, 0.0)) for k in order])
    s.text(x0, 204, "0", 11, "muted")
    s.text((x0 + x1) / 2, 204, "gzip stops at the first bad sector; gpuzip rescue restarts after each one", 11, "muted", "middle")
    s.text(x1, 204, f"{size / 1e6:.0f} MB of output", 11, "muted", "end")
    s.legend(x0 + 60, 222, [("swatch", "exact", "exact"), ("swatch", "inferred", "inferred from its copies"),
                            ("swatch", "suspect", "in a damaged block"), ("swatch", "unknown", "unknown"),
                            ("swatch", "lost", "not recovered")])
    return s


def zip_speed_chart(t):
    d = json.load(open(os.path.join(HERE, "results.json"), encoding="utf-8"))
    names = {"small": "40,000 small files, 1.1 GB", "medium": "2,000 medium files, 1.4 GB",
             "large": "8 large files, 1.1 GB", "mixed": "10,304 mixed files, 0.8 GB"}
    get = {(r["dataset"], r["tool"]): r["median_s"] for r in d["results"] if r["kind"] == "test"}
    rows = [(names[k], get[(k, "7-Zip")], get[(k, "rust cpu")]) for k in names]
    W, x0, top, group = 900, 214, 108, 52
    xmax = 6.0
    pw = W - x0 - 70
    H = top + group * len(rows) + 54
    s = Svg(W, H, t, "Testing ZIP archives, no GPU needed",
            "Seconds to decompress and check every file of four ZIP archives: 7-Zip 24.09 against gpuzip on the CPU.")
    header(s, "Testing ZIP archives: 14-20x faster than 7-Zip, no GPU needed",
           "Seconds to decompress and check every CRC (process start included; lower is better)")
    s.legend(24, 86, [("swatch", "other", "7-Zip 24.09"), ("swatch", "accent", "gpuzip, CPU only")])
    y_end = top + group * len(rows) - 8
    for k in range(0, 7):
        x = x0 + pw * k / xmax
        s.line(x, top - 6, x, y_end, "grid")
        s.text(x, y_end + 16, f"{k} s", 11, "muted", "middle")
    for i, (name, seven, ours) in enumerate(rows):
        y = top + group * i
        s.text(x0 - 14, y + 22, name, 12, "ink2", "end")
        for j, (v, fill, who) in enumerate(((seven, "other", "7-Zip"), (ours, "accent", "gpuzip"))):
            yy = y + j * 20
            s.bar(x0, yy, pw * v / xmax, 18, fill, f"{name}: {who} {v:.2f} s")
            s.text(x0 + pw * v / xmax + 6, yy + 13, f"{v:.2f} s" + (f"  ({seven / ours:.0f}x)" if fill == "accent" else ""),
                   11.5, "ink2" if fill == "other" else "ink", weight=400 if fill == "other" else 600)
    s.text(24, H - 16, "Ryzen 7 9700X (8 cores), Windows 11, median of 3. With the GPU (the default auto mode) "
           "0.22-0.39 s. Data: bench/results.json.", 11, "muted")
    return s


def gzip_speed_chart(t):
    d = json.load(open(os.path.join(HERE, "results-gz.json"), encoding="utf-8"))["datasets"]
    panels = [("big6.txt.gz", "one 6.4 GB .gz (text)"), ("medium.tar.gz", "a 1.4 GB tar.gz of 2,000 files")]
    tools = [("GNU gzip", "GNU gzip 1.13"), ("7-Zip", "7-Zip 24.09"), ("gpuzip cpu", "gpuzip, CPU"),
             ("rapidgzip, 16 threads", "rapidgzip, 16 threads"), ("gpuzip auto", "gpuzip, GPU")]
    W, x0, top, row = 900, 176, 112, 26
    gap = 56
    pw = (W - x0 - 64 - gap) / 2
    H = top + row * len(tools) + 50
    s = Svg(W, H, t, "Testing gzip files",
            "Seconds to decompress and check gzip files: GNU gzip, 7-Zip, rapidgzip and gpuzip on the CPU and on the GPU.")
    header(s, "One large gzip stream, decoded in parallel on the GPU",
           "Seconds to decompress and check the CRC (process start and GPU set-up included; lower is better)")
    for i, (_, label) in enumerate(tools):
        s.text(x0 - 14, top + row * i + 17, label, 12, "ink2", "end")
    for p, (key, title) in enumerate(panels):
        jobs = d[key]["jobs"]["test"]
        px = x0 + p * (pw + gap)
        xmax = 30 if p == 0 else 7
        step = 10 if p == 0 else 2
        y_end = top + row * len(tools)
        s.text(px, top - 14, title, 12, "ink", weight=600)
        for k in range(0, xmax + 1, step):
            x = px + pw * k / xmax
            s.line(x, top - 4, x, y_end, "grid")
            s.text(x, y_end + 16, f"{k} s", 11, "muted", "start" if k == 0 else "middle")
        for i, (key_t, label) in enumerate(tools):
            v = jobs[key_t]
            ours = key_t.startswith("gpuzip")
            y = top + row * i + 4
            s.bar(px, y, pw * v / xmax, 18, "accent" if ours else "other", f"{title}: {label} {v:.2f} s")
            s.text(px + pw * v / xmax + 6, y + 13, f"{v:.2f} s", 11.5, "ink" if ours else "ink2", weight=600 if ours else 400)
    s.text(24, H - 14, "RTX 5080, Ryzen 7 9700X, Windows 11, median of 3. On small files (128 MB) rapidgzip is "
           "still ahead: 0.15 s against 0.19 s. Data: bench/results-gz.json.", 11, "muted")
    return s


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--out", default=os.path.join(HERE, "..", "docs", "img"))
    a = ap.parse_args()
    os.makedirs(a.out, exist_ok=True)
    for name, fn in (("rescue", rescue_chart), ("rescue-map", rescue_map_chart),
                     ("zip-speed", zip_speed_chart), ("gzip-speed", gzip_speed_chart)):
        for mode, t in THEMES.items():
            path = os.path.join(a.out, f"{name}-{mode}.svg")
            fn(t).save(path)
            print(path)


if __name__ == "__main__":
    main()
