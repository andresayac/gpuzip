"""Harder inputs for the prototype: each case is a .deflate stream plus the bytes it decodes to.

  python make_hard_inputs.py <inputs-folder>

  mixed     text with 1 MB stretches of random bytes (zlib stores those as stored blocks)
  repeats   a 64-byte pattern repeated (long chains of back-references)
  small     the first 1 MB of the text (few block starts)
  libdeflate-l12  the text at libdeflate's highest level (other block sizes), if the `deflate`
            package (libdeflate bindings) is installed
"""

import os
import random
import sys
import zlib


def raw_deflate(data, level):
    c = zlib.compressobj(level, zlib.DEFLATED, -15)
    return c.compress(data) + c.flush()


def write(folder, name, data, comp):
    with open(os.path.join(folder, f"{name}.bin"), "wb") as f:
        f.write(data)
    with open(os.path.join(folder, f"{name}.deflate"), "wb") as f:
        f.write(comp)
    assert zlib.decompress(comp, -15) == data
    print(f"{name:<16} {len(data):>12,} bytes -> {len(comp):>12,}")


def main():
    folder = sys.argv[1]
    with open(os.path.join(folder, "text.bin"), "rb") as f:
        text = f.read()
    rnd = random.Random(7)
    parts = []
    for i in range(32):
        parts.append(text[i * (2 << 20):i * (2 << 20) + (2 << 20)])
        parts.append(rnd.randbytes(1 << 20))
    mixed = b"".join(parts)
    write(folder, "mixed", mixed, raw_deflate(mixed, 6))
    pattern = bytes(rnd.randrange(256) for _ in range(64))
    repeats = pattern * ((64 << 20) // 64)
    write(folder, "repeats", repeats, raw_deflate(repeats, 6))
    small = text[:1 << 20]
    write(folder, "small", small, raw_deflate(small, 6))
    try:
        import deflate
        write(folder, "libdeflate-l12", text, deflate.deflate_compress(text, 12))
    except ImportError:
        print("libdeflate-l12   skipped: pip install deflate")


if __name__ == "__main__":
    main()
