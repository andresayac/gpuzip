"""Time the CPU state of the art on the same streams: rapidgzip (parallel speculative gzip
decompression, all cores) and zlib-ng (one thread).

  python vs_rapidgzip.py <inputs-folder> [stream names...]

Each raw Deflate stream is wrapped as a .gz (gzip header + stream + CRC32 + size) so rapidgzip
can read it; the decoded bytes are checked against text.bin.
"""

import os
import struct
import sys
import time
import zlib

import rapidgzip


def best_of(n, fn):
    times = []
    for _ in range(n):
        t = time.perf_counter()
        out = fn()
        times.append(time.perf_counter() - t)
    return min(times), out


def main():
    folder = sys.argv[1]
    names = sys.argv[2:] or ["7zip-mx5", "zlib-ng-l1", "zlib-ng-l6", "zlib-ng-l9"]
    with open(os.path.join(folder, "text.bin"), "rb") as f:
        expect = f.read()
    threads = os.cpu_count()
    print(f"rapidgzip {rapidgzip.__version__}, {threads} threads")
    for name in names:
        with open(os.path.join(folder, f"{name}.deflate"), "rb") as f:
            raw = f.read()
        gz = os.path.join(folder, f"{name}.gz")
        if not os.path.exists(gz):
            with open(gz, "wb") as f:
                f.write(b"\x1f\x8b\x08\x00\x00\x00\x00\x00\x00\xff" + raw)
                f.write(struct.pack("<II", zlib.crc32(expect), len(expect) & 0xFFFFFFFF))

        def rapid():
            with rapidgzip.open(gz, parallelization=threads) as r:
                return r.read()

        rapid()  # warm the file cache and the thread pool
        t_rapid, out = best_of(3, rapid)
        assert out == expect, name
        t_zlib, out = best_of(3, lambda: zlib.decompress(raw, -15))
        assert out == expect, name
        print(f"{name:<12} rapidgzip {threads} threads: {len(expect) / t_rapid / 1e9:5.2f} GB/s"
              f"   zlib-ng 1 thread: {len(expect) / t_zlib / 1e9:5.2f} GB/s")


if __name__ == "__main__":
    main()
