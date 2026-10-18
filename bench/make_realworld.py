"""Builds the generated half of the real-world corpus for realworld.py: archives as other tools
and odd situations make them, beside the public downloads (see realworld.py).

  python bench/make_realworld.py <dir> [--quick]

ZIP: 7-Zip at -mx9, AES-256, ZipCrypto, Deflate64, bzip2, LZMA; Python's zipfile stored and
Zstandard; Windows PowerShell; Info-ZIP's zip (through WSL, if there); a JAR (the JDK's jar,
if there); ZIP64 by count (70,000 entries) and by size (one 4.5 GB entry); data descriptors (a
streamed ZIP); 1 MB of data before the archive (a self-extractor's layout); a 60 KB comment; an
empty archive and empty files; two large entries that hardly compress; names Windows refuses or
mangles; entries that try to leave the output folder.
gzip: three members, every optional header field, 1 GB that does not compress, 200 MB in 64 KB
members (bgzip's layout), a tiny file; a tar.gz with long names, links and an empty folder.
--quick leaves out the two largest (4.5 GB and 1 GB).
"""

import argparse
import gzip
import io
import os
import random
import shutil
import struct
import subprocess
import sys
import tarfile
import time
import zipfile
import zlib

HERE = os.path.dirname(os.path.abspath(__file__))
sys.path.insert(0, os.path.join(HERE, "..", "tests"))
from test_gpuzip import SEVENZIP, make_source  # noqa: E402

PASSWORD = "correct horse"


_WORDS = []


def text_bytes(n, seed):
    if not _WORDS:  # one vocabulary for every file: made once
        w = random.Random(0)
        _WORDS.extend("".join(w.choice("abcdefghijklmnopqrstuvwxyz") for _ in range(w.randint(2, 10))) for _ in range(5000))
    rnd, words = random.Random(seed), _WORDS
    out, size = [], 0
    while size < n:
        line = " ".join(rnd.choice(words) for _ in range(rnd.randint(4, 16))) + "\n"
        out.append(line)
        size += len(line)
    return "".join(out).encode()[:n]


def hard_bytes(n, seed):
    """Data that compresses to about 0.8, like a ROM image: random runs between short text"""
    rnd = random.Random(seed)
    words = text_bytes(1024, seed)
    out = bytearray()
    while len(out) < n:
        out += rnd.randbytes(3072) + words
    return bytes(out[:n])


def seven(*args, cwd=None):
    target = args[-2]  # every call here: a [switches] <archive> <what>
    if os.path.exists(target):
        os.remove(target)  # 7-Zip would update it, not make it anew
    subprocess.run([SEVENZIP, *args], cwd=cwd, check=True, stdout=subprocess.DEVNULL)


def wsl_path(p):
    p = os.path.abspath(p).replace("\\", "/")
    return f"/mnt/{p[0].lower()}{p[2:]}" if p[1] == ":" else p


class NoSeek(io.RawIOBase):
    """A write-only stream that cannot seek: zipfile then writes data descriptors"""

    def __init__(self, f):
        self.f = f

    def writable(self):
        return True

    def write(self, b):
        return self.f.write(b)


def step(name):
    print(f"  {name}", flush=True)


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("dir")
    ap.add_argument("--quick", action="store_true")
    ap.add_argument("--resume", action="store_true", help="keep the small archives already made")
    a = ap.parse_args()
    out = os.path.abspath(a.dir)
    os.makedirs(out, exist_ok=True)
    t0 = time.time()
    src = os.path.join(out, "_source")
    if not os.path.isdir(src):
        os.makedirs(src)
        make_source(src)
    z = lambda name: os.path.join(out, name)

    step("7-Zip variants")
    if all(os.path.exists(z(n)) for n in ("7z_mx9.zip", "7z_aes256.zip", "7z_lzma.zip", "py_comment.zip", "java.jar")) and a.resume:
        print("    already made: skipping to ZIP64")
    else:
        small_archives(src, z)
    big_archives(a, out, z)
    print(f"done in {time.time() - t0:.0f} s: {out}")


def small_archives(src, z):
    seven("a", "-tzip", "-mx=9", z("7z_mx9.zip"), ".", cwd=src)
    seven("a", "-tzip", f"-p{PASSWORD}", "-mem=AES256", z("7z_aes256.zip"), ".", cwd=src)
    seven("a", "-tzip", f"-p{PASSWORD}", "-mem=ZipCrypto", z("7z_zipcrypto.zip"), ".", cwd=src)
    for m in ("Deflate64", "BZip2", "LZMA"):
        seven("a", "-tzip", f"-mm={m}", z(f"7z_{m.lower()}.zip"), ".", cwd=src)

    step("Python zipfile: stored, Zstandard, streamed, prepended, comment")
    files = [(os.path.join(dp, f), os.path.relpath(os.path.join(dp, f), src)) for dp, _, fs in os.walk(src) for f in fs]
    with zipfile.ZipFile(z("py_stored.zip"), "w", zipfile.ZIP_STORED) as zf:
        for p, n in files:
            zf.write(p, n)
    if hasattr(zipfile, "ZIP_ZSTANDARD"):
        with zipfile.ZipFile(z("py_zstd.zip"), "w", zipfile.ZIP_ZSTANDARD) as zf:
            for p, n in files:
                zf.write(p, n)
    with open(z("py_streamed.zip"), "wb") as f, zipfile.ZipFile(NoSeek(f), "w", zipfile.ZIP_DEFLATED) as zf:
        for p, n in files:
            with open(p, "rb") as g, zf.open(n, "w") as w:
                shutil.copyfileobj(g, w)
    with open(z("7z_mx9.zip"), "rb") as f:
        body = f.read()
    with open(z("py_prepended.zip"), "wb") as f:  # a self-extractor's stub, then the archive
        f.write(random.Random(3).randbytes(1 << 20) + body)
    shutil.copy(z("7z_mx9.zip"), z("py_comment.zip"))
    with zipfile.ZipFile(z("py_comment.zip"), "a") as zf:
        zf.comment = b"a long archive comment " * 2700

    step("Windows PowerShell, Info-ZIP, jar")
    if os.name == "nt":
        subprocess.run(["powershell", "-NoProfile", "-Command",
                        f"Compress-Archive -Path '{src}\\*' -DestinationPath '{z('powershell.zip')}' -Force"], check=True)
    if shutil.which("wsl"):
        if os.path.exists(z("infozip.zip")):
            os.remove(z("infozip.zip"))
        r = subprocess.run(["wsl", "-d", "Ubuntu", "--exec", "bash", "-c",  # --exec: no second shell to re-parse it
                            f"export PATH=~/gz-tools/root/usr/bin:$PATH; cd '{wsl_path(src)}' && zip -r -q '{wsl_path(z('infozip.zip'))}' ."],
                           capture_output=True)
        if r.returncode:
            print("    Info-ZIP not available in WSL: skipped")
    jar = shutil.which("jar")
    if jar:
        subprocess.run([jar, "--create", "--file", z("java.jar"), "-C", src, "."], check=True)


def big_archives(a, out, z):
    step("ZIP64 by count: 70,000 entries")
    rnd = random.Random(7)
    with zipfile.ZipFile(z("zip64_70k.zip"), "w", zipfile.ZIP_DEFLATED) as zf:
        for i in range(70000):
            zf.writestr(f"d{i % 97:02}/f{i:05}.txt", text_bytes(rnd.randint(0, 3000), i))

    step("two large entries that hardly compress (300 MB each)")
    with zipfile.ZipFile(z("hard_2x300mb.zip"), "w", zipfile.ZIP_DEFLATED, compresslevel=6) as zf:
        for i in range(2):
            zf.writestr(f"image{i}.img", hard_bytes(300 << 20, 10 + i))

    step("empty archive, empty files")
    with zipfile.ZipFile(z("empty.zip"), "w"):
        pass
    with zipfile.ZipFile(z("zero_bytes.zip"), "w", zipfile.ZIP_DEFLATED) as zf:
        for i in range(5):
            zf.writestr(f"empty{i}.txt", b"")
        zf.writestr("dir/", b"")

    step("names Windows refuses or mangles; entries that try to leave the folder")
    with zipfile.ZipFile(z("names.zip"), "w") as zf:
        for n in ("CON.txt", "nul", "aux.c", "COM1.log", "file:colon.txt", "trailing.", "space ", "q?uestion.txt",
                  "a" * 120 + "/" + "b" * 120 + "/" + "c" * 60 + ".txt", "Readme.md", "README.md",
                  "caf\u00e9.txt", "cafe\u0301.txt", "dir\\back\\slash.txt", "emoji \U0001F600.txt"):
            zf.writestr(n, f"name: {n}".encode())
    with zipfile.ZipFile(z("slip.zip"), "w") as zf:
        for n in ("../evil1.txt", "/abs/evil2.txt", "C:\\evil3.txt", "..\\evil4.txt", "a/../../evil5.txt",
                  "ok/fine.txt"):
            zf.writestr(zipfile.ZipInfo(n), b"x")

    step("gzip: three members, header fields, bgzip layout, tiny")
    parts = [text_bytes(5 << 20, 20 + i) for i in range(3)]
    with open(z("three_members.txt.gz"), "wb") as f:
        for p in parts:
            f.write(gzip.compress(p, 6))
    body = text_bytes(8 << 20, 30)
    c = zlib.compressobj(6, zlib.DEFLATED, -15)
    deflated = c.compress(body) + c.flush()
    extra = b"AB" + struct.pack("<H", 6) + b"gpuzip"
    flags = 0x02 | 0x04 | 0x08 | 0x10  # FHCRC FEXTRA FNAME FCOMMENT
    header = b"\x1f\x8b\x08" + bytes([flags]) + struct.pack("<I", 1760000000) + b"\x00\x03"
    header += struct.pack("<H", len(extra)) + extra + b"header.txt\x00" + b"every optional field\x00"
    header += struct.pack("<H", zlib.crc32(header) & 0xFFFF)
    with open(z("header_fields.txt.gz"), "wb") as f:
        f.write(header + deflated + struct.pack("<II", zlib.crc32(body), len(body)))
    data = text_bytes(200 << 20, 40)
    with open(z("bgzip_layout.txt.gz"), "wb") as f:
        for i in range(0, len(data), 64 << 10):
            f.write(gzip.compress(data[i:i + (64 << 10)], 6))
    with open(z("tiny.txt.gz"), "wb") as f:
        f.write(gzip.compress(b"tiny\n"))

    step("tar.gz with long names, links, an empty folder")
    tree = os.path.join(out, "_tartree")
    shutil.rmtree(tree, ignore_errors=True)
    os.makedirs(os.path.join(tree, "empty folder"))
    deep = os.path.join(tree, "d" * 90, "e" * 90)
    os.makedirs(deep)
    with open(os.path.join(deep, "f" * 80 + ".txt"), "wb") as f:
        f.write(text_bytes(100000, 50))
    with open(os.path.join(tree, "a\u00f1o 2026 \u6587\u4ef6.txt"), "wb") as f:
        f.write(b"unicode name\n")
    with tarfile.open(z("pax_links.tar.gz"), "w:gz", format=tarfile.PAX_FORMAT) as t:
        t.add(tree, "root")
        info = tarfile.TarInfo("root/link-to-file")
        info.type, info.linkname = tarfile.SYMTYPE, "root/a\u00f1o 2026 \u6587\u4ef6.txt"
        t.addfile(info)
        info = tarfile.TarInfo("root/hardlink")
        info.type, info.linkname = tarfile.LNKTYPE, "root/a\u00f1o 2026 \u6587\u4ef6.txt"
        t.addfile(info)
    shutil.rmtree(tree)

    if not a.quick:
        step("1 GB gzip that does not compress")
        with open(z("random_1gb.bin.gz"), "wb") as f:
            g = gzip.GzipFile(fileobj=f, mode="wb", compresslevel=1)
            r = random.Random(60)
            for _ in range(64):
                g.write(r.randbytes(16 << 20))
            g.close()
        step("ZIP64 by size: one 4.5 GB entry")
        with zipfile.ZipFile(z("zip64_4p5gb.zip"), "w", zipfile.ZIP_DEFLATED, compresslevel=1) as zf:
            with zf.open("big.txt", "w", force_zip64=True) as w:
                block = text_bytes(64 << 20, 70)
                for i in range(72):  # 4.5 GB
                    w.write(block[i % 7:] + block[: i % 7])


if __name__ == "__main__":
    main()
