"""Hostile archives against gpuzip: it must refuse or contain them, never crash, write outside
the output folder, run out of memory, or report success with wrong data.

  python bench/malicious.py --bin PATH [--work DIR] [--big]

ZIP: entries that overlap (one compressed stream named many times, the "better zip bomb" idea);
a size field that says 1 KB for 100 MB of data, and one that says 4 GB or 1 TB for 1 KB; a wrong
CRC; a local header offset past the end of the file; a directory that claims 65,535 entries.
tar.gz: a symbolic link out of the folder followed by a file written through it, an absolute
link, a hard link out of the folder, names with ../ and absolute names.
gzip: a 2 GB stream of zeros in 2 MB (with --big, 8 GB), extracted and rescued, with the peak
memory of the gpuzip process measured.
"""

import argparse
import gzip
import json
import io
import os
import shutil
import struct
import subprocess
import sys
import tarfile
import tempfile
import threading
import time
import zlib

results = []


def record(name, ok, detail):
    results.append({"case": name, "ok": bool(ok), "detail": detail})
    print(f"{'ok  ' if ok else 'FAIL'} {name}: {detail}", flush=True)


def deflate(data, level=9):
    c = zlib.compressobj(level, zlib.DEFLATED, -15)
    return c.compress(data) + c.flush()


def local_header(name, comp, crc, csize, usize, method=8):
    n = name.encode()
    return struct.pack("<IHHHHHIIIHH", 0x04034B50, 20, 0x800, method, 0, 0x21, crc, csize, usize, len(n), 0) + n + comp


def central(name, crc, csize, usize, offset, method=8, extra=b""):
    n = name.encode()
    return struct.pack("<IHHHHHHIIIHHHHHII", 0x02014B50, 0x031E, 20, 0x800, method, 0, 0x21, crc, csize, usize,
                       len(n), len(extra), 0, 0, 0, 0o100644 << 16, offset) + n + extra


def eocd(count, cd_size, cd_offset, claimed=None):
    c = claimed if claimed is not None else count
    return struct.pack("<IHHHHIIH", 0x06054B50, 0, 0, c, c, cd_size, cd_offset, 0)


def build(entries_local, entries_cd, claimed=None):
    body = b"".join(entries_local)
    cd = b"".join(entries_cd)
    return body + cd + eocd(len(entries_cd), len(cd), len(body), claimed)


class Peak:
    """Peak working set of a process, sampled (Windows: tasklist; elsewhere /proc)"""

    def __init__(self, pid):
        self.pid, self.peak, self.stop = pid, 0, False
        threading.Thread(target=self.run, daemon=True).start()

    def sample(self):
        try:
            if os.name == "nt":
                out = subprocess.run(["tasklist", "/FI", f"PID eq {self.pid}", "/FO", "CSV", "/NH"], capture_output=True,
                                     text=True).stdout
                field = out.strip().split('","')[-1].strip('"').replace(",", "").replace(".", "").split()[0]
                return int(field) * 1024
            with open(f"/proc/{self.pid}/status") as f:
                for line in f:
                    if line.startswith("VmRSS:"):
                        return int(line.split()[1]) * 1024
        except Exception:  # noqa: BLE001
            return 0
        return 0

    def run(self):
        while not self.stop:
            self.peak = max(self.peak, self.sample())
            time.sleep(0.2)


def run(cmd, timeout=900):
    t = time.perf_counter()
    p = subprocess.Popen(cmd, stdout=subprocess.PIPE, stderr=subprocess.STDOUT, text=True, encoding="utf-8", errors="replace")
    peak = Peak(p.pid)
    try:
        out, _ = p.communicate(timeout=timeout)
    except subprocess.TimeoutExpired:
        p.kill()
        out, _ = p.communicate()
        out += "\n[timed out]"
    peak.stop = True
    return p.returncode, out, time.perf_counter() - t, peak.peak


def outside_files(sandbox):
    return sorted(f for f in os.listdir(sandbox) if f != "out" and not f.startswith("archive"))


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--bin", required=True)
    ap.add_argument("--work", default=os.path.join(tempfile.gettempdir(), "gpuzip-bench", "malicious"))
    ap.add_argument("--big", action="store_true")
    a = ap.parse_args()
    g = os.path.abspath(a.bin)
    shutil.rmtree(a.work, ignore_errors=True)
    os.makedirs(a.work)

    def case(name, data, ext, cmd="x", check=None, timeout=900):
        sandbox = os.path.join(a.work, f"case{len(results):02}")
        os.makedirs(sandbox)
        path = os.path.join(sandbox, "archive" + ext)
        open(path, "wb").write(data)
        out = os.path.join(sandbox, "out")
        rc, text, secs, peak = run([g, cmd, path, "-o", out], timeout)
        got = {}
        for dp, _, fs in os.walk(out):
            for f in fs:
                p = os.path.join(dp, f)
                got[os.path.relpath(p, out).replace("\\", "/")] = os.path.getsize(p) if not os.path.islink(p) else -1
        info = {"rc": rc, "seconds": round(secs, 1), "peak_mb": peak >> 20, "outside": outside_files(sandbox),
                "written": dict(list(got.items())[:6]), "total_written_mb": sum(v for v in got.values() if v > 0) >> 20,
                "tail": text.strip()[-220:]}
        crashed = "panicked" in text or rc not in (0, 1, 2)
        ok = not crashed and not info["outside"] and (check(rc, got, info) if check else True)
        record(name, ok, info)
        shutil.rmtree(sandbox, ignore_errors=True)

    # ZIP: one compressed stream named by many entries (overlap): 20 x 100 MB from 100 KB
    zeros = bytes(100 << 20)
    comp, crc = deflate(zeros), zlib.crc32(zeros)
    lh = local_header("kernel.bin", comp, crc, len(comp), len(zeros))
    cds = [central(f"copy{i:02}.bin", crc, len(comp), len(zeros), 0) for i in range(20)]
    case("zip: 20 entries overlapping one 100 MB stream", build([lh], cds), ".zip",
         check=lambda rc, got, info: True)  # recorded for what it does: a bomb is extracted or refused

    # ZIP: sizes that lie
    data = os.urandom(1 << 20) + bytes(99 << 20)
    comp, crc = deflate(data, 6), zlib.crc32(data)
    z = build([local_header("liar.bin", comp, crc, len(comp), 1024)], [central("liar.bin", crc, len(comp), 1024, 0)])
    case("zip: says 1 KB, holds 100 MB", z, ".zip", check=lambda rc, got, info: rc == 2 and got.get("liar.bin", 0) <= 1024)
    small = b"tiny" * 256
    comp, crc = deflate(small), zlib.crc32(small)
    z = build([local_header("big.bin", comp, crc, len(comp), 0xFFFFFFFE)], [central("big.bin", crc, len(comp), 0xFFFFFFFE, 0)])
    case("zip: says 4 GB, holds 1 KB", z, ".zip", check=lambda rc, got, info: rc == 2 and info["peak_mb"] < 6000)
    z64 = struct.pack("<HHQQ", 1, 16, 1 << 40, len(comp))  # Zip64 extra: uncompressed 1 TB, compressed size
    ld = local_header("huge.bin", comp, crc, len(comp), len(small))
    cd = struct.pack("<IHHHHHHIIIHHHHHII", 0x02014B50, 0x031E, 45, 0x800, 8, 0, 0x21, crc, len(comp), 0xFFFFFFFF,
                     8, len(z64), 0, 0, 0, 0, 0) + b"huge.bin" + z64
    case("zip64: says 1 TB, holds 1 KB", build([ld], [cd]), ".zip", check=lambda rc, got, info: rc in (1, 2) and info["peak_mb"] < 6000)

    # ZIP: a wrong CRC, an offset past the end, a directory that claims 65,535 entries
    comp, crc = deflate(small), zlib.crc32(small)
    z = build([local_header("crc.bin", comp, crc ^ 1, len(comp), len(small))], [central("crc.bin", crc ^ 1, len(comp), len(small), 0)])
    case("zip: wrong CRC", z, ".zip", check=lambda rc, got, info: rc == 2)
    z = build([local_header("far.bin", comp, crc, len(comp), len(small))], [central("far.bin", crc, len(comp), len(small), 10 ** 9)])
    case("zip: local header offset past the end", z, ".zip", check=lambda rc, got, info: rc in (1, 2))
    z = build([local_header("one.bin", comp, crc, len(comp), len(small))], [central("one.bin", crc, len(comp), len(small), 0)], claimed=65535)
    case("zip: directory claims 65,535 entries, holds 1", z, ".zip", check=lambda rc, got, info: rc in (0, 1, 2))

    # tar.gz: links and names that leave the folder
    def tgz(members):
        buf = io.BytesIO()
        with tarfile.open(fileobj=buf, mode="w:gz") as t:
            for kind, name, payload in members:
                info = tarfile.TarInfo(name)
                if kind == "sym":
                    info.type, info.linkname = tarfile.SYMTYPE, payload
                    t.addfile(info)
                elif kind == "hard":
                    info.type, info.linkname = tarfile.LNKTYPE, payload
                    t.addfile(info)
                else:
                    info.size = len(payload)
                    t.addfile(info, io.BytesIO(payload))
        return buf.getvalue()

    escape = lambda rc, got, info: not any(k.startswith("..") for k in got)
    case("tar: symlink out of the folder, then a file through it",
         tgz([("sym", "evil", "../../"), ("file", "evil/pwned.txt", b"pwned"), ("file", "ok.txt", b"ok")]), ".tar.gz",
         check=escape)
    case("tar: absolute symlink, then a file through it",
         tgz([("sym", "abs", os.path.abspath(a.work)), ("file", "abs/pwned2.txt", b"pwned"), ("file", "ok.txt", b"ok")]),
         ".tar.gz", check=lambda rc, got, info: not os.path.exists(os.path.join(a.work, "pwned2.txt")))
    case("tar: hard link to a file out of the folder",
         tgz([("hard", "link", "../../outside.txt"), ("file", "ok.txt", b"ok")]), ".tar.gz", check=escape)
    case("tar: ../ and absolute names",
         tgz([("file", "../up.txt", b"x"), ("file", "/abs.txt", b"x"), ("file", "ok.txt", b"ok")]), ".tar.gz", check=escape)

    # gzip: zeros in bulk, extracted and rescued: memory must stay bounded
    gb = 8 if a.big else 2
    buf = io.BytesIO()
    with gzip.GzipFile(fileobj=buf, mode="wb", compresslevel=9) as f:
        chunk = bytes(64 << 20)
        for _ in range(gb * 16):
            f.write(chunk)
    bomb = buf.getvalue()
    print(f"  ({gb} GB of zeros in {len(bomb) >> 10} KB)")
    case(f"gzip: {gb} GB of zeros, extracted", bomb, ".bin.gz",
         check=lambda rc, got, info: rc == 0 and info["peak_mb"] < 2000)
    case(f"gzip: {gb} GB of zeros, rescued", bomb, ".bin.gz", cmd="rescue",
         check=lambda rc, got, info: info["peak_mb"] < 4000)
    case(f"tar.gz: {gb} GB of zeros in one file, rescued", _tar_of_zeros(gb).getvalue(), ".tar.gz", cmd="rescue",
         check=lambda rc, got, info: info["peak_mb"] < 4000)

    path = os.path.join(a.work, "malicious.json")
    json.dump(results, open(path, "w"), indent=1)
    bad = [r for r in results if not r["ok"]]
    print(f"\n{len(results) - len(bad)} of {len(results)} ok; {path}")
    return 1 if bad else 0


def _tar_of_zeros(gb):
    """A tar.gz of one file of zeros, streamed (never the whole tar in memory)"""
    buf = io.BytesIO()
    with gzip.GzipFile(fileobj=buf, mode="wb", compresslevel=9) as gz:
        info = tarfile.TarInfo("zeros.bin")
        info.size = gb << 30
        gz.write(info.tobuf(format=tarfile.GNU_FORMAT))
        chunk = bytes(64 << 20)
        for _ in range(gb * 16):
            gz.write(chunk)
        gz.write(bytes(1024))  # end of archive
    return buf


if __name__ == "__main__":
    sys.exit(main())
