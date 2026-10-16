"""Small raw Deflate streams of every block kind, for gi_test fuzz.

    python make_fuzz_inputs.py <text.bin> <out-dir>

text.bin: any text (the prototype's 128 MB text works; its first 2 MB are used).
"""
import os
import random
import sys
import zlib


def deflate(data, level=6, strategy=zlib.Z_DEFAULT_STRATEGY):
    c = zlib.compressobj(level, zlib.DEFLATED, -15, 9, strategy)
    return c.compress(data) + c.flush()


def main():
    text = open(sys.argv[1], "rb").read(2 << 20)
    out = sys.argv[2]
    os.makedirs(out, exist_ok=True)
    rng = random.Random(7)
    mixed = bytearray()
    while len(mixed) < (1 << 20):
        k = rng.randrange(3)
        n = rng.randrange(1, 40000)
        if k == 0:
            mixed += rng.randbytes(n)  # random: stored blocks
        elif k == 1:
            a = rng.randrange(len(text) - n)
            mixed += text[a:a + n]
        else:
            mixed += bytes([rng.randrange(256)]) * n  # long runs
    streams = {
        "f-l1": deflate(text[: 1 << 20], 1),
        "f-l6": deflate(text, 6),
        "f-l9": deflate(text[: 1 << 20], 9),
        "f-fixed": deflate(text[: 1 << 20], 6, zlib.Z_FIXED),
        "f-stored": deflate(text[: 512 << 10], 0),
        "f-mixed": deflate(bytes(mixed), 6),
        "f-huffman": deflate(text[: 1 << 20], 6, zlib.Z_HUFFMAN_ONLY),
        "f-tiny": deflate(b"hello, hello, hello world\n" * 3, 6),
    }
    for name, data in streams.items():
        open(os.path.join(out, name + ".deflate"), "wb").write(data)
        print(f"{name:10} {len(data):>9} bytes")


main()
