"""The byte map of one rescue, for charts.py: what gzip gives of a damaged .gz against what
gpuzip rescue gives, binned along the output.

  python bench/rescue_map.py <damaged.gz> <rescue.mask> <original> <out.json> [--label TEXT] [--bins 360]

<rescue.mask> is gzrescue's --mask output (research/recovery: one byte per output byte, 1 exact,
3 inferred, 2 in a damaged block, 0 unknown). gzip's share is the prefix of its output that
matches the original: after the first damaged block it decodes garbage, or stops.
"""

import argparse
import json
import os
import zlib

NAMES = {1: "exact", 3: "inferred", 2: "suspect", 0: "unknown"}


def gzip_prefix(gz_path, original):
    """How many leading bytes gzip decodes right before damage stops it or turns it to garbage"""
    d, got = zlib.decompressobj(31), bytearray()
    with open(gz_path, "rb") as f:
        try:
            while chunk := f.read(1 << 20):
                got += d.decompress(chunk)
                if len(got) > len(original):
                    break
        except zlib.error:
            pass
    n, step = 0, 1 << 16
    while n < len(got) and got[n:n + step] == original[n:n + step]:  # whole steps, then byte by byte
        n += step
    n = min(n, len(got), len(original))
    while n > 0 and got[:n] != original[:n]:
        n -= 1
    while n < min(len(got), len(original)) and got[n] == original[n]:
        n += 1
    return n


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("damaged")
    ap.add_argument("mask")
    ap.add_argument("original")
    ap.add_argument("out")
    ap.add_argument("--label", default="")
    ap.add_argument("--bins", type=int, default=360)
    a = ap.parse_args()
    original = open(a.original, "rb").read()
    mask = open(a.mask, "rb").read()
    size = len(mask)
    bins = []
    for b in range(a.bins):
        lo, hi = size * b // a.bins, size * (b + 1) // a.bins
        part = mask[lo:hi]
        bins.append({NAMES[k]: round(part.count(k) / max(1, len(part)), 4) for k in NAMES if part.count(k)})
    totals = {NAMES[k]: mask.count(k) for k in NAMES}
    result = {
        "label": a.label or os.path.basename(a.damaged),
        "damaged_size": os.path.getsize(a.damaged),
        "output_size": size,
        "gzip_prefix": gzip_prefix(a.damaged, original),
        "totals": totals,
        "bins": bins,
    }
    json.dump(result, open(a.out, "w"), indent=0)
    share = {k: f"{100 * v / size:.2f}%" for k, v in totals.items()}
    print(f"{a.out}: gzip {100 * result['gzip_prefix'] / size:.2f}% · rescue {share}")


if __name__ == "__main__":
    main()
