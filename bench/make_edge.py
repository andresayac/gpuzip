"""Edge cases of names, formats and metadata, made and checked against a gpuzip.

  python bench/make_edge.py --bin PATH [--work DIR]

Names: ZIPs from Japanese, Chinese and Russian Windows (Shift-JIS, GBK, CP866 names, no UTF-8
flag), which 7-Zip reads with -mcp. Formats: a ZIP split by Info-ZIP (.z01 ... .zip, through
WSL), 7-Zip volumes (.7z.001), a 7-Zip self-extractor (.exe), a wrong password, encrypted and
plain entries in one ZIP. Metadata: Unix permissions (the executable bit) in a ZIP and a tar.gz,
the read-only attribute, folder times, Info-ZIP's extended time (UTC, to the second) against
the DOS time (local, two-second steps). Each case says what gpuzip did; where the right answer
is clear, whether it was right.
"""

import argparse
import io
import json
import os
import shutil
import stat
import struct
import subprocess
import sys
import tarfile
import tempfile
import time
import zipfile

HERE = os.path.dirname(os.path.abspath(__file__))
sys.path.insert(0, os.path.join(HERE, "..", "tests"))
from test_gpuzip import SEVENZIP  # noqa: E402

results = []


def record(name, ok, detail):
    results.append({"case": name, "ok": ok, "detail": detail})
    mark = "ok  " if ok is True else ("FAIL" if ok is False else "info")
    print(f"{mark} {name}: {detail}", flush=True)


def gpuzip(exe, *args):
    r = subprocess.run([exe, *args, "--json"], capture_output=True, text=True, encoding="utf-8", errors="replace")
    rep = {}
    for line in reversed(r.stdout.strip().splitlines()):
        try:
            rep = json.loads(line)
            break
        except json.JSONDecodeError:
            continue
    return r.returncode, rep, (r.stdout + r.stderr).strip()[-300:]


def listing(root):
    out = []
    for dp, ds, fs in os.walk(root):
        for f in fs:
            out.append(os.path.relpath(os.path.join(dp, f), root).replace("\\", "/"))
    return sorted(out)


def raw_name_zip(path, names, create_system=0):
    """A ZIP whose names are the given raw bytes, with no UTF-8 flag (zipfile cannot write those)"""
    buf = io.BytesIO()
    placeholders = []
    with zipfile.ZipFile(buf, "w", zipfile.ZIP_DEFLATED) as z:
        for i, raw in enumerate(names):
            ph = (f"{i:03}" + "x" * len(raw))[: len(raw)]  # ASCII, the same length
            placeholders.append((ph.encode(), raw))
            info = zipfile.ZipInfo(ph, date_time=(2026, 1, 2, 3, 4, 6))
            info.create_system = create_system
            info.compress_type = zipfile.ZIP_DEFLATED
            z.writestr(info, f"file {i}\n".encode() * 50)
    data = buf.getvalue()
    for ph, raw in placeholders:
        assert data.count(ph) == 2, ph  # local header and central directory
        data = data.replace(ph, raw)
    open(path, "wb").write(data)


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--bin", required=True)
    ap.add_argument("--work", default=os.path.join(tempfile.gettempdir(), "gpuzip-bench", "edge"))
    a = ap.parse_args()
    g = os.path.abspath(a.bin)
    w = a.work
    shutil.rmtree(w, ignore_errors=True)
    os.makedirs(w)
    out = lambda n: os.path.join(w, "out-" + n)

    # names in legacy code pages
    for label, cp, names in (("Japanese (Shift-JIS)", "cp932", ["日本語のファイル.txt", "資料/報告書.txt"]),
                             ("Chinese (GBK)", "gbk", ["中文文件.txt", "文档/报告.txt"]),
                             ("Russian DOS (CP866)", "cp866", ["Документ.txt", "Папка/отчёт.txt"])):
        p = os.path.join(w, f"names-{cp}.zip")
        raw_name_zip(p, [n.encode(cp) for n in names])
        rc, rep, tail = gpuzip(g, "x", p, "-o", out(cp))
        got = listing(out(cp))
        right = sorted(n for n in names)
        r7 = subprocess.run([SEVENZIP, "x", p, f"-o{out(cp)}-7z", f"-mcp={cp[2:] if cp.startswith('cp') else '936'}", "-y"],
                            capture_output=True)
        got7 = listing(out(cp) + "-7z")
        record(f"names from {label}, no UTF-8 flag", got == right,
               f"gpuzip wrote {got}; 7-Zip -mcp wrote {got7}")

    # split and multi-volume archives, a self-extractor
    src = os.path.join(w, "src")
    os.makedirs(src)
    for i in range(6):
        open(os.path.join(src, f"part{i}.bin"), "wb").write(os.urandom(3 << 20))
    split = os.path.join(w, "split", "archive.zip")
    os.makedirs(os.path.dirname(split))
    if os.name == "nt":  # Info-ZIP through WSL
        to_wsl = lambda p: "/mnt/" + p[0].lower() + p[2:].replace("\\", "/")
        r = subprocess.run(["wsl", "-d", "Ubuntu", "--exec", "bash", "-c",
                            f"export PATH=~/gz-tools/root/usr/bin:$PATH; cd '{to_wsl(src)}' && zip -q -s 5m -r '{to_wsl(split)}' ."],
                           capture_output=True)
    else:
        r = subprocess.run(["zip", "-q", "-s", "5m", "-r", split, "."], cwd=src, capture_output=True)
    if r.returncode == 0:
        parts = sorted(os.listdir(os.path.dirname(split)))
        rc, rep, tail = gpuzip(g, "x", split, "-o", out("split"))
        record("ZIP split by Info-ZIP (.z01 ... .zip)", rc == 0 and len(listing(out("split"))) == 6 or None,
               f"parts {parts}; exit {rc}, wrote {len(listing(out('split')))} of 6: {tail[-160:]}")
    vol = os.path.join(w, "vol", "archive.7z")
    os.makedirs(os.path.dirname(vol))
    subprocess.run([SEVENZIP, "a", "-v5m", vol, os.path.join(src, "*")], capture_output=True, check=True)
    rc, rep, tail = gpuzip(g, "x", vol + ".001", "-o", out("vol"))
    record("7-Zip volumes (.7z.001)", rc == 0 and len(listing(out("vol"))) == 6, f"exit {rc}, wrote {len(listing(out('vol')))} of 6: {tail[-160:]}")
    sfx = os.path.join(w, "setup.exe")
    r = subprocess.run([SEVENZIP, "a", "-sfx", sfx, os.path.join(src, "*")], capture_output=True)
    if r.returncode == 0:
        rc, rep, tail = gpuzip(g, "x", sfx, "-o", out("sfx"))
        record("7-Zip self-extractor (.exe)", None, f"exit {rc}, wrote {len(listing(out('sfx')))} of 6: {tail[-160:]}")

    # passwords
    enc = os.path.join(w, "aes.zip")
    subprocess.run([SEVENZIP, "a", "-tzip", "-pright", "-mem=AES256", enc, os.path.join(src, "part0.bin")], capture_output=True, check=True)
    rc, rep, tail = gpuzip(g, "x", enc, "-o", out("wrongpw"), "--password", "wrong")
    left = listing(out("wrongpw"))
    record("wrong password", rc == 2 and not any(os.path.getsize(os.path.join(out("wrongpw"), f)) for f in left),
           f"exit {rc}, files left {left}: {tail[-160:]}")
    rc, rep, tail = gpuzip(g, "x", enc, "-o", out("nopw"))
    record("password needed, none given", rc == 2, f"exit {rc}: {tail[-160:]}")
    mixed = os.path.join(w, "mixed.zip")
    subprocess.run([SEVENZIP, "a", "-tzip", mixed, os.path.join(src, "part1.bin")], capture_output=True, check=True)
    subprocess.run([SEVENZIP, "a", "-tzip", "-pright", mixed, os.path.join(src, "part2.bin")], capture_output=True, check=True)
    rc, rep, tail = gpuzip(g, "x", mixed, "-o", out("mixed"), "--password", "right")
    record("encrypted and plain entries, password given", rc == 0 and len(listing(out("mixed"))) == 2, f"exit {rc}: {tail[-120:]}")
    rc, rep, tail = gpuzip(g, "x", mixed, "-o", out("mixed-nopw"))
    record("encrypted and plain entries, no password", None,
           f"exit {rc}, wrote {listing(out('mixed-nopw'))}: {tail[-160:]}")

    # metadata
    meta = os.path.join(w, "meta.zip")
    with zipfile.ZipFile(meta, "w", zipfile.ZIP_DEFLATED) as z:
        for name, mode, dt in (("run.sh", 0o100755, (2024, 5, 6, 7, 8, 10)), ("data.txt", 0o100644, (2024, 5, 6, 7, 8, 10)),
                               ("folder/", 0o040755, (2020, 1, 1, 0, 0, 0))):
            info = zipfile.ZipInfo(name, date_time=dt)
            info.create_system, info.external_attr = 3, mode << 16
            z.writestr(info, b"" if name.endswith("/") else b"#!/bin/sh\necho hi\n")
        info = zipfile.ZipInfo("readonly.txt", date_time=(2024, 5, 6, 7, 8, 10))
        info.create_system, info.external_attr = 0, 0x01  # MS-DOS attributes: read-only
        z.writestr(info, b"read only\n")
        # Info-ZIP's extended time: UTC, to the second; the DOS time says something else
        info = zipfile.ZipInfo("utime.txt", date_time=(2024, 1, 1, 0, 0, 0))
        info.extra = struct.pack("<HHBI", 0x5455, 5, 1, 1700000001)
        z.writestr(info, b"extended time\n")
    rc, rep, tail = gpuzip(g, "x", meta, "-o", out("meta"))
    o = out("meta")
    st = lambda f: os.stat(os.path.join(o, f))
    if os.name != "nt":
        record("Unix executable bit kept (ZIP)", bool(st("run.sh").st_mode & 0o111), oct(st("run.sh").st_mode))
    ro = not os.access(os.path.join(o, "readonly.txt"), os.W_OK)
    record("read-only attribute kept", ro, f"writable: {not ro}")
    ut = st("utime.txt").st_mtime
    record("extended time (UT) preferred to the DOS time", abs(ut - 1700000001) < 2,
           f"mtime {time.strftime('%Y-%m-%d %H:%M:%S', time.gmtime(ut))} UTC, the UT field says 2023-11-14 22:13:21 UTC")
    fd = time.localtime(st("folder").st_mtime)
    record("folder time kept", fd.tm_year == 2020, time.strftime("%Y-%m-%d %H:%M", fd))
    tgz = os.path.join(w, "meta.tar.gz")
    with tarfile.open(tgz, "w:gz") as t:
        info = tarfile.TarInfo("tool")
        info.mode, info.size, info.mtime = 0o755, 10, 1600000000
        t.addfile(info, io.BytesIO(b"#!/bin/sh\n"))
        info = tarfile.TarInfo("dir")
        info.type, info.mode, info.mtime = tarfile.DIRTYPE, 0o755, 1500000000
        t.addfile(info)
    rc, rep, tail = gpuzip(g, "x", tgz, "-o", out("metatar"))
    ot = out("metatar")
    if os.name != "nt":
        record("Unix executable bit kept (tar)", bool(os.stat(os.path.join(ot, "tool")).st_mode & 0o111), oct(os.stat(os.path.join(ot, "tool")).st_mode))
    record("tar file time kept", abs(os.stat(os.path.join(ot, "tool")).st_mtime - 1600000000) < 2,
           time.strftime("%Y-%m-%d %H:%M:%S", time.gmtime(os.stat(os.path.join(ot, "tool")).st_mtime)))
    record("tar folder time kept", abs(os.stat(os.path.join(ot, "dir")).st_mtime - 1500000000) < 2,
           time.strftime("%Y-%m-%d %H:%M:%S", time.gmtime(os.stat(os.path.join(ot, "dir")).st_mtime)))

    path = os.path.join(w, "edge.json")
    json.dump(results, open(path, "w", encoding="utf-8"), indent=1, ensure_ascii=False)
    bad = [r for r in results if r["ok"] is False]
    print(f"\n{sum(1 for r in results if r['ok'] is True)} right, {len(bad)} wrong, "
          f"{sum(1 for r in results if r['ok'] is None)} for information; {path}")
    return 1 if bad else 0


if __name__ == "__main__":
    sys.exit(main())
