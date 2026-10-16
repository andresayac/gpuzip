"""Checks a .zst written by gpuzstd with an independent decoder (Python 3.14's compression.zstd):
    python check_zst.py <original> <file.zst>
"""
import hashlib
import sys
from compression import zstd

orig = open(sys.argv[1], "rb").read()
comp = open(sys.argv[2], "rb").read()
out = bytearray()
d = zstd.ZstdDecompressor()
pos = 0
while pos < len(comp):  # one frame per chunk; a decompressor object handles one frame
    out += d.decompress(comp[pos:])
    pos = len(comp) - len(d.unused_data)
    d = zstd.ZstdDecompressor()
same = hashlib.sha256(out).digest() == hashlib.sha256(orig).digest()
print(f"{sys.argv[2]}: {len(comp)} bytes -> {len(out)} bytes, ratio {len(orig) / len(comp):.3f}, {'identical to the original' if same else 'DIFFERENT'}")
sys.exit(0 if same else 1)
