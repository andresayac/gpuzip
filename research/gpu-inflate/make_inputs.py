"""Test inputs for the speculative GPU inflate prototype: single large raw Deflate streams and
the bytes they decode to, plus the time zlib-ng (CPU, one thread) needs for each.

  python make_inputs.py <bench-folder> <out-folder>

From <bench-folder>/large.zip (built by bench/bench.py, packed by 7-Zip) it takes the first
entry (128 MB of text), writes its raw Deflate bytes as 7zip-mx5.deflate and the decoded bytes
as text.bin, then recompresses text.bin with zlib-ng at levels 1, 6 and 9.
"""

import os
import struct
import sys
import time
import zipfile
import zlib


def raw_deflate(data, level):
    c = zlib.compressobj(level, zlib.DEFLATED, -15)
    return c.compress(data) + c.flush()


def main():
    bench, out = sys.argv[1], sys.argv[2]
    os.makedirs(out, exist_ok=True)
    path = os.path.join(bench, "large.zip")
    with zipfile.ZipFile(path) as z, open(path, "rb") as f:
        info = next(i for i in z.infolist() if not i.is_dir() and i.compress_type == zipfile.ZIP_DEFLATED)
        f.seek(info.header_offset)
        header = f.read(30)
        name_len, extra_len = struct.unpack_from("<HH", header, 26)
        f.seek(info.header_offset + 30 + name_len + extra_len)
        comp = f.read(info.compress_size)
        data = z.read(info)
    streams = {"7zip-mx5": comp}
    for level in (1, 6, 9):
        streams[f"zlib-ng-l{level}"] = raw_deflate(data, level)
    with open(os.path.join(out, "text.bin"), "wb") as f:
        f.write(data)
    print(f"text.bin: {len(data):,} bytes ({info.filename} from large.zip)")
    for name, s in streams.items():
        with open(os.path.join(out, f"{name}.deflate"), "wb") as f:
            f.write(s)
        best = min(timed(lambda: zlib.decompress(s, -15)) for _ in range(3))
        assert zlib.decompress(s, -15) == data
        print(f"{name + '.deflate':<22} {len(s):>12,} bytes  ratio {len(data) / len(s):4.2f}"
              f"  zlib-ng 1 thread: {len(data) / best / 1e9:.2f} GB/s")


def timed(fn):
    t = time.perf_counter()
    fn()
    return time.perf_counter() - t


if __name__ == "__main__":
    main()
