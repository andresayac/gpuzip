"""A 1 GB tar of System32 binaries (files of up to 64 MB), for timing the compressor over
several 128 MB chunks.

    python make_big.py <out.tar> [MB=1024]
"""
import io
import os
import sys
import tarfile


def main():
    out = sys.argv[1]
    size = int(sys.argv[2] if len(sys.argv) > 2 else 1024) << 20
    root = os.path.join(os.environ.get("SystemRoot", r"C:\Windows"), "System32")
    with open(out, "wb") as g:
        t = tarfile.open(fileobj=g, mode="w")
        written = 0
        for dirpath, dirs, files in os.walk(root):
            dirs.sort()
            for name in sorted(files):
                p = os.path.join(dirpath, name)
                try:
                    st = os.stat(p)
                    if st.st_size == 0 or st.st_size > (64 << 20):
                        continue
                    with open(p, "rb") as f:
                        data = f.read()
                except OSError:
                    continue
                info = tarfile.TarInfo(os.path.relpath(p, root).replace("\\", "/"))
                info.size = len(data)
                t.addfile(info, io.BytesIO(data))
                written = g.tell()
                if written >= size:
                    t.close()
                    g.truncate(size)
                    print(f"{out}: {size >> 20} MB")
                    return
        t.close()
    print(f"{out}: {os.path.getsize(out) >> 20} MB (all of System32's files under 64 MB)")


main()
