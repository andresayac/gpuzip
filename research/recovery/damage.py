"""Makes a gzip file from an original and damaged copies of it, for the recovery experiments.

    python damage.py <original> <out-dir> [--level 6]

Writes <name>.gz (zlib, gzip container, with the file name in the header) and these copies:
  zero4k      4 KB of zeros at 50% of the compressed size (a bad sector)
  flip1       one bit flipped at 30%
  rand1k      1 KB overwritten with random bytes at 70%
  trunc       cut at 60% (an interrupted download)
  two         512 zero bytes at 25% and a flipped bit at 75%
  header      the first 16 bytes zeroed (the gzip header and the first block's start)
and damage.json with what was done where.
"""
import json
import os
import random
import sys
import zlib


def main():
    src, out = sys.argv[1], sys.argv[2]
    level = int(sys.argv[3]) if len(sys.argv) > 3 else 6
    os.makedirs(out, exist_ok=True)
    name = os.path.basename(src)
    data = open(src, "rb").read()
    c = zlib.compressobj(level, zlib.DEFLATED, 31)  # 31: gzip container
    gz = c.compress(data) + c.flush()
    # a header with FNAME, like gzip writes
    gz = bytes([0x1F, 0x8B, 8, 8, 0, 0, 0, 0, 0, 0xFF]) + name.encode() + b"\0" + gz[10:]
    open(os.path.join(out, name + ".gz"), "wb").write(gz)
    n = len(gz)
    rng = random.Random(1)
    meta = {"original": os.path.abspath(src), "original_size": len(data), "gz_size": n, "cases": {}}

    def write(case, b, desc):
        open(os.path.join(out, f"{name}.{case}.gz"), "wb").write(bytes(b))
        meta["cases"][case] = desc

    b = bytearray(gz)
    p = n // 2
    b[p:p + 4096] = bytes(4096)
    write("zero4k", b, {"zeroed": [p, 4096]})
    b = bytearray(gz)
    p = int(n * 0.3)
    b[p] ^= 1 << 3
    write("flip1", b, {"bit_flipped": [p, 3]})
    b = bytearray(gz)
    p = int(n * 0.7)
    b[p:p + 1024] = rng.randbytes(1024)
    write("rand1k", b, {"random": [p, 1024]})
    write("trunc", gz[: int(n * 0.6)], {"truncated_at": int(n * 0.6)})
    b = bytearray(gz)
    p1, p2 = n // 4, int(n * 0.75)
    b[p1:p1 + 512] = bytes(512)
    b[p2] ^= 1 << 5
    write("two", b, {"zeroed": [p1, 512], "bit_flipped": [p2, 5]})
    b = bytearray(gz)
    b[0:16] = bytes(16)
    write("header", b, {"zeroed": [0, 16]})
    json.dump(meta, open(os.path.join(out, "damage.json"), "w"), indent=1)
    print(f"{name}: {len(data)} bytes -> {n} bytes gzip; 6 damaged copies in {out}")


main()
