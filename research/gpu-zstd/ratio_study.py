"""How much ratio is there to gain within the zstd format, and at what CPU cost?

    python ratio_study.py <corpus-dir> [--workers 12] [--out results.json]

For every file of the corpus (see make_corpus.py): zstd at levels 3, 9 and 19; level 19 with
the window limited to 64 KB and 1 MB (what a block-local GPU compressor would see), with a
128 MB window plus long-distance matching, and with a near-exhaustive match search (on the
first 64 MB: it is slow); level 22; and 7-Zip's LZMA2 at -mx9 as the ceiling of a stronger
format. Everything runs in parallel processes; speeds are per thread (7-Zip: 4 threads).
"""
import argparse
import json
import os
import subprocess
import sys
import tempfile
import time
from concurrent.futures import ProcessPoolExecutor
from compression import zstd
from compression.zstd import CompressionParameter as P

SEVENZIP = r"C:\Program Files\7-Zip\7z.exe"

CONFIGS = [
    ("zstd -3", {P.compression_level: 3}, None),
    ("zstd -9", {P.compression_level: 9}, None),
    ("zstd -19", {P.compression_level: 19}, None),
    ("zstd -19, window 64 KB", {P.compression_level: 19, P.window_log: 16}, None),
    ("zstd -19, window 1 MB", {P.compression_level: 19, P.window_log: 20}, None),
    ("zstd -19, window 128 MB + LDM", {P.compression_level: 19, P.window_log: 27, P.enable_long_distance_matching: 1}, None),
    ("zstd -22, window 128 MB + LDM", {P.compression_level: 22, P.window_log: 27, P.enable_long_distance_matching: 1}, None),
    ("zstd -19, search x16 (64 MB)", {P.compression_level: 19, P.search_log: 11, P.target_length: 4096}, 64 << 20),
    ("LZMA2 -mx9 (7-Zip)", None, None),
]


def run(path, name, opts, limit):
    with open(path, "rb") as f:
        data = f.read(limit) if limit else f.read()
    t = time.perf_counter()
    if opts is None:
        out = os.path.join(tempfile.gettempdir(), f"ratio-{os.getpid()}.7z")
        subprocess.run([SEVENZIP, "a", "-t7z", "-mx9", "-mmt4", out, path], check=True, stdout=subprocess.DEVNULL)
        n = os.path.getsize(out)
        os.remove(out)
    else:
        n = len(zstd.compress(data, options=opts))
    secs = time.perf_counter() - t
    return os.path.basename(path), name, len(data) / n, secs, len(data) / secs / 1e6


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("corpus")
    ap.add_argument("--workers", type=int, default=12)
    ap.add_argument("--out", default=os.path.join(os.path.dirname(os.path.abspath(__file__)), "ratio_study.json"))
    a = ap.parse_args()
    files = sorted(os.path.join(a.corpus, n) for n in os.listdir(a.corpus))
    jobs = [(p, name, opts, limit) for name, opts, limit in CONFIGS for p in files]
    jobs.sort(key=lambda j: 0 if j[1].startswith(("zstd -19, search", "LZMA2", "zstd -22")) else 1)  # slow ones first
    results = []
    with ProcessPoolExecutor(a.workers) as pool:
        for r in pool.map(run, *zip(*jobs)):
            results.append(r)
            print(f"{r[0]:14} {r[1]:32} ratio {r[2]:6.2f}  {r[3]:7.1f} s  {r[4]:7.1f} MB/s", flush=True)
    with open(a.out, "w") as f:
        json.dump(results, f, indent=1)
    names = [c[0] for c in CONFIGS]
    print("\n| Dataset | " + " | ".join(names) + " |")
    print("|---|" + "---|" * len(names))
    for p in files:
        base = os.path.basename(p)
        row = {r[1]: r[2] for r in results if r[0] == base}
        print(f"| {base} | " + " | ".join(f"{row.get(n, 0):.2f}" for n in names) + " |")


if __name__ == "__main__":
    main()
