"""Builds a corpus of real data for the compression experiments, from files already on this
machine (nothing personal: system logs, open-source code, program binaries) plus the synthetic
text of the earlier benchmarks.

    python make_corpus.py <out-dir> [MB=128]

Writes one file per dataset, each `MB` large:
  logs.txt      Windows CBS log (machine text, long lines, timestamps)
  setup.txt     Windows setup log (another log style)
  code.tar      Rust crate sources, Python's standard library, CUDA headers
  binaries.tar  DLLs and EXEs from System32
  mixed.tar     a web stack's install (Laragon: PHP, MySQL, Apache...): binaries, text, data
  text.bin      the synthetic English-like text of the gpuzip benchmarks (if found)
"""
import io
import os
import shutil
import sys
import tarfile


def head(src, dst, size):
    try:
        with open(src, "rb") as f, open(dst, "wb") as g:
            g.write(f.read(size))
    except OSError as e:
        print(f"skipping {src}: {e}")


def tar_sample(roots, dst, size, max_file=16 << 20):
    """A tar of the files under roots, sorted, up to `size` bytes, skipping very large files."""
    buf = io.BytesIO()
    t = tarfile.open(fileobj=buf, mode="w")
    for root in roots:
        for dirpath, dirs, files in os.walk(root):
            dirs.sort()
            for name in sorted(files):
                p = os.path.join(dirpath, name)
                try:
                    st = os.stat(p)
                except OSError:
                    continue
                if st.st_size > max_file or st.st_size == 0:
                    continue
                try:
                    with open(p, "rb") as f:
                        data = f.read()
                except OSError:
                    continue
                info = tarfile.TarInfo(os.path.relpath(p, root).replace("\\", "/"))
                info.size = len(data)
                info.mtime = int(st.st_mtime)
                t.addfile(info, io.BytesIO(data))
                if buf.tell() >= size:
                    t.close()
                    with open(dst, "wb") as g:
                        g.write(buf.getvalue()[:size])
                    return
    t.close()
    with open(dst, "wb") as g:
        g.write(buf.getvalue()[:size])


def main():
    out = sys.argv[1]
    size = int(sys.argv[2] if len(sys.argv) > 2 else 128) << 20
    os.makedirs(out, exist_ok=True)
    win = os.environ.get("SystemRoot", r"C:\Windows")
    home = os.path.expanduser("~")
    cbs = sorted((os.path.join(win, "Logs", "CBS", n) for n in os.listdir(os.path.join(win, "Logs", "CBS"))
                  if n.endswith(".log")), key=os.path.getsize, reverse=True)
    if cbs:
        head(cbs[0], os.path.join(out, "logs.txt"), size)
    setup = os.path.join(win, "Panther", "setupact.log")
    if os.path.exists(setup):
        head(setup, os.path.join(out, "setup.txt"), size)
    code = [p for p in (os.path.join(home, ".cargo", "registry", "src"), r"C:\Python314\Lib",
                        os.path.join(os.environ.get("CUDA_PATH", ""), "include")) if os.path.isdir(p)]
    tar_sample(code, os.path.join(out, "code.tar"), size)
    tar_sample([os.path.join(win, "System32")], os.path.join(out, "binaries.tar"), size)
    if os.path.isdir(r"C:\laragon\bin"):
        tar_sample([r"C:\laragon\bin"], os.path.join(out, "mixed.tar"), size)
    text = os.path.join(os.environ.get("TEMP", ""), "gpuzip-bench", "inflate", "text.bin")
    if os.path.exists(text):
        head(text, os.path.join(out, "text.bin"), size)
    for n in sorted(os.listdir(out)):
        print(f"{n:14} {os.path.getsize(os.path.join(out, n)) >> 20:4} MB")


main()
