"""Mutation fuzzing of gpuzip's archive readers: ZIP (several producers, Zip64, data descriptors),
tar.gz and multi-member gzip, each damaged at random, then tested, extracted and rescued.

  python bench/fuzz_archives.py --bin PATH [--iterations 3000] [--seed 1] [--jobs 8] [--work DIR]

What counts as a failure: a panic, an exit code other than 0, 1 or 2, a run longer than its time
limit, a file written outside the output folder, or `t` saying 0 while `x` of the same input
fails (the two must agree). Every failing input is kept in WORK/failures with its command.
"""

import argparse
import gzip
import io
import json
import multiprocessing as mp
import os
import random
import shutil
import struct
import subprocess
import sys
import tarfile
import tempfile
import zipfile

TIMEOUT = 30


def seeds():
    rnd = random.Random(0)
    words = [bytes(rnd.choice(b"abcdefghij") for _ in range(rnd.randint(2, 8))) for _ in range(300)]
    files = {f"dir{i % 3}/file{i}.txt": b" ".join(rnd.choice(words) for _ in range(rnd.randint(0, 3000))) for i in range(12)}
    files["bin/blob.bin"] = rnd.randbytes(20000)
    out = {}
    for name, method in (("deflate.zip", zipfile.ZIP_DEFLATED), ("stored.zip", zipfile.ZIP_STORED)):
        buf = io.BytesIO()
        with zipfile.ZipFile(buf, "w", method) as z:
            for n, d in files.items():
                z.writestr(n, d)
        out[name] = buf.getvalue()
    buf = io.BytesIO()
    with zipfile.ZipFile(buf, "w", zipfile.ZIP_DEFLATED) as z:  # Zip64 records on every entry
        for n, d in files.items():
            with z.open(n, "w", force_zip64=True) as w:
                w.write(d)
    out["zip64.zip"] = buf.getvalue()

    class NoSeek(io.RawIOBase):
        def __init__(self, f):
            self.f = f

        def writable(self):
            return True

        def write(self, b):
            return self.f.write(b)

    buf = io.BytesIO()
    with zipfile.ZipFile(NoSeek(buf), "w", zipfile.ZIP_DEFLATED) as z:  # data descriptors
        for n, d in files.items():
            with z.open(n, "w") as w:
                w.write(d)
    out["streamed.zip"] = buf.getvalue()
    buf = io.BytesIO()
    with tarfile.open(fileobj=buf, mode="w:gz") as t:
        for n, d in files.items():
            info = tarfile.TarInfo(n)
            info.size = len(d)
            t.addfile(info, io.BytesIO(d))
    out["files.tar.gz"] = buf.getvalue()
    out["members.txt.gz"] = b"".join(gzip.compress(d) for d in list(files.values())[:5])
    return out


def mutate(data, rnd):
    b = bytearray(data)
    for _ in range(rnd.choice((1, 1, 2, 3, 8))):
        op = rnd.random()
        p = rnd.randrange(len(b)) if b else 0
        if op < 0.3:  # flip bits
            b[p] ^= 1 << rnd.randrange(8)
        elif op < 0.5:  # overwrite a few bytes with something meaningful to a parser
            v = rnd.choice((b"\x00", b"\xff", b"\xff\xff\xff\xff", b"\x00\x00\x00\x00", b"PK\x01\x02", b"PK\x03\x04",
                            b"PK\x05\x06", b"PK\x06\x06", b"\x1f\x8b\x08", b"..", b"/", b"\\"))
            b[p:p + len(v)] = v
        elif op < 0.65:  # a field near a ZIP signature: sizes, offsets, counts, name lengths
            sig = rnd.choice((b"PK\x01\x02", b"PK\x03\x04", b"PK\x05\x06"))
            at = b.find(sig, rnd.randrange(len(b)))
            if at >= 0 and at + 46 < len(b):
                q = at + rnd.choice((10, 12, 14, 16, 18, 20, 22, 24, 26, 28, 30, 32, 34, 42))
                b[q:q + 4] = struct.pack("<I", rnd.choice((0, 1, 0xFFFF, 0xFFFFFFFF, rnd.randrange(1 << 32))))
        elif op < 0.75:  # cut
            del b[rnd.randrange(len(b)):]
        elif op < 0.85:  # a stretch copied elsewhere
            n = rnd.randint(1, 4096)
            src = rnd.randrange(len(b))
            b[p:p] = b[src:src + n]
        else:  # zeros, as a bad sector leaves them
            n = rnd.randint(1, 4096)
            b[p:p + n] = bytes(min(n, len(b) - p))
    return bytes(b)


def one(task):
    exe, work, seed_name, data, i = task
    sandbox = os.path.join(work, f"w{os.getpid()}")
    shutil.rmtree(sandbox, ignore_errors=True)
    os.makedirs(sandbox)
    ext = ".tar.gz" if seed_name.endswith(".tar.gz") else os.path.splitext(seed_name)[1]
    path = os.path.join(sandbox, "input" + ext)
    open(path, "wb").write(data)
    problems, rcs = [], {}
    for cmd in ("t", "x", "rescue"):
        args = [exe, cmd, path] + ([] if cmd == "t" else ["-o", os.path.join(sandbox, "out")])
        try:
            r = subprocess.run(args, capture_output=True, text=True, encoding="utf-8", errors="replace", timeout=TIMEOUT)
            rcs[cmd] = r.returncode
            text = r.stdout + r.stderr
            if "panicked" in text or r.returncode not in (0, 1, 2):
                problems.append(f"{cmd}: exit {r.returncode}: {text.strip()[-300:]}")
        except subprocess.TimeoutExpired:
            problems.append(f"{cmd}: no answer in {TIMEOUT} s")
        outside = [f for f in os.listdir(sandbox) if f not in ("out", os.path.basename(path))]
        if outside:
            problems.append(f"{cmd}: wrote outside the output folder: {outside}")
        shutil.rmtree(os.path.join(sandbox, "out"), ignore_errors=True)
    if rcs.get("t") == 0 and rcs.get("x") not in (0, None):
        problems.append(f"t says 0 but x says {rcs.get('x')}")
    if problems:
        keep = os.path.join(work, "failures")
        os.makedirs(keep, exist_ok=True)
        name = f"{i:05}-{seed_name}"
        open(os.path.join(keep, name), "wb").write(data)
        return {"input": name, "problems": problems}
    return None


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--bin", required=True)
    ap.add_argument("--iterations", type=int, default=3000)
    ap.add_argument("--seed", type=int, default=1)
    ap.add_argument("--jobs", type=int, default=max(1, (os.cpu_count() or 4) // 2))
    ap.add_argument("--work", default=os.path.join(tempfile.gettempdir(), "gpuzip-bench", "fuzz-archives"))
    a = ap.parse_args()
    shutil.rmtree(a.work, ignore_errors=True)
    os.makedirs(a.work)
    base = seeds()
    rnd = random.Random(a.seed)
    names = sorted(base)
    tasks = []
    for i in range(a.iterations):
        n = names[i % len(names)]
        tasks.append((os.path.abspath(a.bin), a.work, n, mutate(base[n], rnd), i))
    failures = []
    with mp.Pool(a.jobs) as pool:
        for k, res in enumerate(pool.imap_unordered(one, tasks, chunksize=4), 1):
            if res:
                failures.append(res)
                print(f"FAIL {res['input']}: {res['problems'][0][:240]}", flush=True)
            if k % 500 == 0:
                print(f"  {k} of {len(tasks)}, {len(failures)} failures", flush=True)
    json.dump(failures, open(os.path.join(a.work, "failures.json"), "w"), indent=1)
    print(f"\n{len(tasks)} inputs ({', '.join(names)}), 3 commands each: {len(failures)} failures; {a.work}")
    return 1 if failures else 0


if __name__ == "__main__":
    sys.exit(main())
