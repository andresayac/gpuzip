"""Reproduces the test suite's damaged tar.gz case and shows what gpuzip rescue reports.

    python repro_test.py <work-dir>
"""
import hashlib
import json
import os
import shutil
import subprocess
import sys
import tarfile

sys.path.insert(0, os.path.join(os.path.dirname(os.path.abspath(__file__)), "..", "..", "tests"))
from test_gpuzip import make_source, tree  # noqa: E402

GPUZIP = os.path.join(os.path.dirname(os.path.abspath(__file__)), "..", "..", "rust", "target", "release", "gpuzip.exe")


def main():
    work = sys.argv[1]
    src = os.path.join(work, "source")
    if not os.path.isdir(src):
        os.makedirs(src)
        make_source(src)
    tgz = os.path.join(work, "archive.tar.gz")
    if not os.path.exists(tgz):
        with tarfile.open(tgz, "w:gz") as t:
            for entry in sorted(os.listdir(src)):
                t.add(os.path.join(src, entry), entry)
    data = bytearray(open(tgz, "rb").read())
    where = float(sys.argv[2]) if len(sys.argv) > 2 else 0.4
    p = int(len(data) * where)
    data[p:p + 2048] = bytes(2048)
    bad = os.path.join(work, "damaged.tar.gz")
    open(bad, "wb").write(bytes(data))
    out = os.path.join(work, "rescued")
    shutil.rmtree(out, ignore_errors=True)
    r = subprocess.run([GPUZIP, "rescue", bad, "-o", out, "--json"], capture_output=True, text=True, encoding="utf-8")
    rep = json.loads(r.stdout.strip().splitlines()[-1])
    print("exit", r.returncode)
    print({k: rep[k] for k in ("damaged_blocks", "crc_check", "length_off", "truncated", "clean", "block_headers", "exact_bytes", "unknown_bytes", "suspect_bytes")})
    print("damaged:", rep["damaged"])
    print("notes:", rep["notes"])
    print("tar:", {k: (v if not isinstance(v, list) else len(v)) for k, v in rep["tar"].items()})
    expected = tree(src)
    got = tree(out)
    for k, v in expected.items():
        if v[0] == "dir":
            continue
        if k not in got:
            print("missing:", k)
        elif got[k][0] != v[0]:
            status = "partial" if k in rep["tar"]["partial"] else "suspect" if k in rep["tar"]["suspect"] else "CALLED INTACT"
            print("differs:", k, status, os.path.getsize(os.path.join(src, k)), "->", os.path.getsize(os.path.join(out, k)))
    # where is that file in the tar, relative to the damage?
    with tarfile.open(tgz) as t:
        for m in t:
            if m.isfile() and m.name in [k for k, v in expected.items() if v[0] != "dir" and k in got and got[k][0] != v[0]]:
                print("  member", m.name, "data offset", m.offset_data, "size", m.size)
    print("damaged output offsets:", [(d["output_offset"], d["length"]) for d in rep["damaged"]])


main()
