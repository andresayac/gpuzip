"""Correctness tests for gpuzip: archives made by different tools must extract to exactly
the files they were made from, in every mode.

  python tests\\test_gpuzip.py                                                (the Python version)
  set GPUZIP_BIN=rust\\target\\release\\gpuzip.exe & python tests\\test_gpuzip.py   (the Rust build)

Builds a source folder (text, binary, empty files, empty folders, non-ASCII names), packs it
with 7-Zip, Python's zipfile and Windows PowerShell, then extracts each archive with gpuzip in
cpu, gpu and auto mode and compares every file's bytes and modification time with the source.
"""

import hashlib
import json
import os
import random
import shutil
import subprocess
import sys
import tarfile
import tempfile
import zipfile

HERE = os.path.dirname(os.path.abspath(__file__))
GPUZIP = [os.path.abspath(os.environ["GPUZIP_BIN"])] if os.environ.get("GPUZIP_BIN") else \
    [sys.executable, os.path.join(HERE, "..", "python", "gpuzip.py")]
SEVENZIP = r"C:\Program Files\7-Zip\7z.exe"
PASSWORD = "correct horse"


def make_source(root):
    rnd = random.Random(42)
    words = ["".join(rnd.choice("abcdefghijklmnopqrstuvwxyz") for _ in range(rnd.randint(2, 10))) for _ in range(2000)]

    def text(n):
        out, size = [], 0
        while size < n:
            w = rnd.choice(words) + rnd.choice("  ,.\n")
            out.append(w)
            size += len(w)
        return "".join(out)[:n].encode()

    dirs = ["src", "src/lib", "docs", "data/raw", "data/año 2026", "naïve 文件"]
    for i in range(400):
        size = min(int(rnd.lognormvariate(9, 1.6)), 3_000_000)
        path = os.path.join(root, dirs[i % len(dirs)], f"file{i:03}.txt")
        os.makedirs(os.path.dirname(path), exist_ok=True)
        with open(path, "wb") as f:
            f.write(text(size))
    for i in range(20):
        with open(os.path.join(root, "data", f"blob{i}.bin"), "wb") as f:
            f.write(rnd.randbytes(rnd.randint(1000, 2_000_000)))
    for i in range(3):
        with open(os.path.join(root, "docs", f"large{i}.log"), "wb") as f:
            f.write(text(6_000_000))
    for i in range(5):
        open(os.path.join(root, "src", f"empty{i}.txt"), "wb").close()
    os.makedirs(os.path.join(root, "empty dir", "nested"))
    for dirpath, _, files in os.walk(root):  # even timestamps survive the 2 s DOS resolution
        for name in files:
            os.utime(os.path.join(dirpath, name), (1_760_000_000, 1_760_000_000))


def tree(root):
    out = {}
    for dirpath, dirs, files in os.walk(root):
        rel_dir = os.path.relpath(dirpath, root)
        if not dirs and not files and rel_dir != ".":
            out[rel_dir.replace("\\", "/") + "/"] = ("dir", 0)
        for name in files:
            p = os.path.join(dirpath, name)
            with open(p, "rb") as f:
                digest = hashlib.sha256(f.read()).hexdigest()
            out[os.path.relpath(p, root).replace("\\", "/")] = (digest, os.path.getmtime(p))
    return out


def same(expected, got, check_times=True):
    problems = []
    for k in sorted(set(expected) | set(got)):
        if k not in got:
            problems.append(f"missing {k}")
        elif k not in expected:
            problems.append(f"extra {k}")
        elif expected[k][0] != got[k][0]:
            problems.append(f"content differs {k}")
        elif check_times and expected[k][0] != "dir" and abs(expected[k][1] - got[k][1]) > 2:
            problems.append(f"mtime differs {k}")
    return problems


def sevenzip(*args, cwd=None):
    subprocess.run([SEVENZIP, *args], cwd=cwd, check=True, stdout=subprocess.DEVNULL)


def build_archives(src, work):
    a = {}
    for level in (1, 5, 9):
        a[f"7-Zip deflate -mx{level}"] = p = os.path.join(work, f"7z_mx{level}.zip")
        sevenzip("a", "-tzip", f"-mx={level}", p, ".", cwd=src)
    a["7-Zip Deflate64 (7-Zip fallback)"] = p = os.path.join(work, "7z_deflate64.zip")
    sevenzip("a", "-tzip", "-mm=Deflate64", p, ".", cwd=src)
    a["7-Zip ZipCrypto password (7-Zip fallback)"] = p = os.path.join(work, "7z_crypto.zip")
    sevenzip("a", "-tzip", f"-p{PASSWORD}", p, ".", cwd=src)

    methods = [zipfile.ZIP_DEFLATED, zipfile.ZIP_STORED, zipfile.ZIP_BZIP2, zipfile.ZIP_LZMA, zipfile.ZIP_ZSTANDARD]
    for name, level, mixed in (("Python zipfile level 1", 1, False), ("Python zipfile level 9", 9, False),
                               ("Python zipfile mixed methods", 6, True)):
        a[name] = p = os.path.join(work, name.replace(" ", "_") + ".zip")
        with zipfile.ZipFile(p, "w", zipfile.ZIP_DEFLATED, compresslevel=level) as z:
            i = 0
            for dirpath, dirs, files in os.walk(src):
                rel = os.path.relpath(dirpath, src)
                if not dirs and not files:
                    z.write(dirpath, rel)
                for f in files:
                    z.write(os.path.join(dirpath, f), os.path.join(rel, f),
                            compress_type=methods[i % len(methods)] if mixed else None)
                    i += 1

    a["Windows PowerShell Compress-Archive"] = p = os.path.join(work, "powershell.zip")
    subprocess.run(["powershell", "-NoProfile", "-Command",
                    f"Compress-Archive -Path '{src}\\*' -DestinationPath '{p}' -CompressionLevel Optimal"], check=True)

    a["7z archive (whole file to 7-Zip)"] = p = os.path.join(work, "archive.7z")
    sevenzip("a", "-t7z", p, ".", cwd=src)
    a["tar.gz (whole file to 7-Zip)"] = p = os.path.join(work, "archive.tar.gz")
    with tarfile.open(p, "w:gz") as t:
        for entry in sorted(os.listdir(src)):
            t.add(os.path.join(src, entry), entry)
    return a


def run_gpuzip(*args):
    r = subprocess.run([*GPUZIP, *args, "--json"], capture_output=True, text=True)
    try:
        return r.returncode, json.loads(r.stdout.strip().splitlines()[-1])
    except (json.JSONDecodeError, IndexError):
        return r.returncode, {"error": r.stdout + r.stderr}


def main():
    work = tempfile.mkdtemp(prefix="gpuzip-test-")
    src = os.path.join(work, "source")
    os.makedirs(src)
    make_source(src)
    expected = tree(src)
    archives = build_archives(src, work)
    failures = 0
    print(f"testing: {' '.join(GPUZIP)}")
    print(f"source: {len(expected)} files and empty folders\n")
    for label, path in archives.items():
        exp = expected
        check_times = "PowerShell" not in label  # .NET Framework stores times its own way
        for mode in ("cpu", "gpu", "gpu-batches", "auto"):
            out = os.path.join(work, "out")
            shutil.rmtree(out, ignore_errors=True)
            extra = ["--password", PASSWORD] if "password" in label else []
            if mode == "gpu-batches":  # 1 MB batches: many round trips to the GPU process
                mode, extra = "gpu", extra + ["--batch-mb", "1"]
            rc, rep = run_gpuzip("x", path, "-o", out, "--mode", mode, "--gpu-min-mb", "0", "-q", *extra)
            problems = same(exp, tree(out), check_times) if "error" not in rep else [rep["error"][-300:]]
            if rc != 0 or rep.get("failed"):
                problems.append(f"exit {rc}, failed: {rep.get('failed')}")
            rc_t, rep_t = run_gpuzip("t", path, "--mode", mode, "--gpu-min-mb", "0", "-q", *extra)
            if rc_t != 0:
                problems.append(f"test command exit {rc_t}: {rep_t.get('failed') or rep_t.get('error', '')[-200:]}")
            routes = rep.get("routes", {})
            where = ", ".join(f"{r} {routes[r]['files']}" for r in ("gpu", "cpu", "7z") if routes.get(r, {}).get("files"))
            status = "ok  " if not problems else "FAIL"
            failures += bool(problems)
            print(f"{status} {label:<42} {mode:<5} {where}")
            for p in problems[:8]:
                print(f"       {p}")

    # zip slip: an entry that tries to write outside the output folder must be refused
    evil = os.path.join(work, "zipslip.zip")
    with zipfile.ZipFile(evil, "w") as z:
        z.writestr("../escaped.txt", "should not exist")
        z.writestr("fine.txt", "ok")
    out = os.path.join(work, "slip_out")
    rc, rep = run_gpuzip("x", evil, "-o", out, "-q")
    escaped = os.path.exists(os.path.join(work, "escaped.txt"))
    ok = rc == 2 and not escaped and os.path.exists(os.path.join(out, "fine.txt"))
    failures += not ok
    print(f"{'ok  ' if ok else 'FAIL'} zip slip entry refused, safe entry extracted")

    shutil.rmtree(work, ignore_errors=True)
    print(f"\n{'All tests passed' if not failures else f'{failures} test(s) failed'}")
    return 1 if failures else 0


if __name__ == "__main__":
    sys.exit(main())
