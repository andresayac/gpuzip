"""A gzip file whose output is larger than 4 GiB (64-bit positions, the size field wrapping,
hundreds of segments), and the SHA-256 of that output.

    python make_big_gz.py <text.bin> <out.gz> [GiB=6]

Each 128 MB piece is the text with a few bytes changed, so the pieces differ.
"""
import hashlib
import sys
import zlib


def main():
    text = bytearray(open(sys.argv[1], "rb").read())
    out = sys.argv[2]
    gib = float(sys.argv[3]) if len(sys.argv) > 3 else 6.0
    total = int(gib * (1 << 30))
    sha = hashlib.sha256()
    c = zlib.compressobj(4, zlib.DEFLATED, 31)  # 31: gzip container
    written = 0
    k = 0
    with open(out, "wb") as f:
        while written < total:
            piece = text[: min(len(text), total - written)]
            for j in range(0, len(piece), 1 << 20):  # a different byte every MB
                piece[j] = (k * 31 + j // (1 << 20)) & 0x7F
            sha.update(piece)
            f.write(c.compress(piece))
            written += len(piece)
            k += 1
            print(f"\r{written / (1 << 30):.2f} GiB", end="", flush=True)
        f.write(c.flush())
    print(f"\n{out}: {written} bytes of output, sha256 {sha.hexdigest()}")


main()
