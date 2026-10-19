"""Correctness tests for gpuzip: archives made by different tools must extract to exactly
the files they were made from, in every mode.

  python tests\\test_gpuzip.py                     (the build in rust\\target\\release)
  GPUZIP_BIN=path/to/gpuzip python3 tests/test_gpuzip.py   (another build)

Builds a source folder (text, binary, empty files, empty folders, non-ASCII names), packs it
with 7-Zip, Python's zipfile and the system's own tool (PowerShell on Windows, Info-ZIP's zip
elsewhere), then extracts each archive with gpuzip in cpu, gpu and auto mode and compares every
file's bytes and modification time with the source. gzip files (one member, several, from
7-Zip, a tar.gz) are checked the same way, and a damaged one must fail in every mode.

Without a GPU that gpuzip can use (`gpuzip doctor` makes no test decode) the gpu modes are
skipped: they would only repeat the CPU fallback. GPUZIP_TEST_GPU=1 or 0 overrides the check.
Archives that need 7-Zip are skipped when it is not installed (7zz, 7z or 7za on PATH).
"""

import gzip
import hashlib
import io
import json
import os
import random
import shutil
import struct
import subprocess
import sys
import tarfile
import tempfile
import time
import zipfile
import zlib

HERE = os.path.dirname(os.path.abspath(__file__))
GPUZIP = [os.path.abspath(os.environ.get("GPUZIP_BIN") or
                          os.path.join(HERE, "..", "rust", "target", "release", "gpuzip.exe" if os.name == "nt" else "gpuzip"))]
SEVENZIP = next((p for p in (shutil.which("7zz"), shutil.which("7z"), shutil.which("7za"), r"C:\Program Files\7-Zip\7z.exe")
                 if p and os.path.isfile(p)), None)
PASSWORD = "correct horse"
SKIPPED = []  # what this machine cannot test, said once at the start


def has_gpu():
    """Whether gpuzip finds a GPU it can decode on: its doctor's test decode"""
    if os.environ.get("GPUZIP_TEST_GPU") in ("0", "1"):
        return os.environ["GPUZIP_TEST_GPU"] == "1"
    r = subprocess.run([*GPUZIP, "doctor"], capture_output=True, text=True, encoding="utf-8", errors="replace")
    return "test decode ok" in r.stdout


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
    if SEVENZIP:
        for level in (1, 5, 9):
            a[f"7-Zip deflate -mx{level}"] = p = os.path.join(work, f"7z_mx{level}.zip")
            sevenzip("a", "-tzip", f"-mx={level}", p, ".", cwd=src)
        a["7-Zip Deflate64 (7-Zip fallback)"] = p = os.path.join(work, "7z_deflate64.zip")
        sevenzip("a", "-tzip", "-mm=Deflate64", p, ".", cwd=src)
        a["7-Zip ZipCrypto password (7-Zip fallback)"] = p = os.path.join(work, "7z_crypto.zip")
        sevenzip("a", "-tzip", f"-p{PASSWORD}", p, ".", cwd=src)
    else:
        SKIPPED.append("archives made by 7-Zip, and the ones only 7-Zip extracts: 7-Zip not found")

    methods = [zipfile.ZIP_DEFLATED, zipfile.ZIP_STORED, zipfile.ZIP_BZIP2, zipfile.ZIP_LZMA]
    if hasattr(zipfile, "ZIP_ZSTANDARD"):  # Python 3.14+
        methods.append(zipfile.ZIP_ZSTANDARD)
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

    if os.name == "nt":
        a["Windows PowerShell Compress-Archive"] = p = os.path.join(work, "powershell.zip")
        subprocess.run(["powershell", "-NoProfile", "-Command",
                        f"Compress-Archive -Path '{src}\\*' -DestinationPath '{p}' -CompressionLevel Optimal"], check=True)
    elif shutil.which("zip"):
        a["Info-ZIP zip -r"] = p = os.path.join(work, "infozip.zip")
        subprocess.run(["zip", "-r", "-q", p, "."], cwd=src, check=True)
    else:
        SKIPPED.append("a ZIP made by the system's own tool: Info-ZIP's zip not found")

    if SEVENZIP:
        a["7z archive (whole file to 7-Zip)"] = p = os.path.join(work, "archive.7z")
        sevenzip("a", "-t7z", p, ".", cwd=src)
    a["tar.gz (Python tarfile)"] = p = os.path.join(work, "archive.tar.gz")
    with tarfile.open(p, "w:gz") as t:
        for entry in sorted(os.listdir(src)):
            t.add(os.path.join(src, entry), entry)
    return a


def build_gzips(src, work):
    """Plain gzip files, each with the one file it must extract to: label -> (path, tree)"""
    logs = [open(os.path.join(src, "docs", f"large{i}.log"), "rb").read() for i in range(3)]
    blob = b"".join(open(os.path.join(src, "data", f"blob{i}.bin"), "rb").read() for i in range(20))
    one = logs[0] + blob + logs[1] * 3  # about 30 MB: big enough for the GPU in auto mode
    digest = lambda b: hashlib.sha256(b).hexdigest()
    g = {}
    p = os.path.join(work, "one.txt.gz")
    with open(p, "wb") as f:
        f.write(gzip.compress(one, 6))
    g["gzip, one member (Python gzip)"] = (p, {"one.txt": (digest(one), 0)})
    p = os.path.join(work, "three.txt.gz")
    with open(p, "wb") as f:
        for part in logs:
            f.write(gzip.compress(part, 9))
    g["gzip, three members"] = (p, {"three.txt": (digest(b"".join(logs)), 0)})
    if SEVENZIP:
        raw = os.path.join(work, "seven.txt")
        with open(raw, "wb") as f:
            f.write(one)
        p = os.path.join(work, "seven.txt.gz")
        sevenzip("a", "-tgzip", "-mx=7", p, raw)
        os.remove(raw)
        g["gzip by 7-Zip -mx7"] = (p, {"seven.txt": (digest(one), 0)})
    return g

def repo_text():
    """This repository's source files: {path: bytes}"""
    root = os.path.join(HERE, "..")
    text = {}
    for sub in ("rust/src", "gpuinflate", "research/recovery", "docs", "tests"):
        d = os.path.join(root, sub)
        for n in sorted(os.listdir(d)) if os.path.isdir(d) else []:
            if n.endswith((".rs", ".cpp", ".cu", ".cuh", ".h", ".py", ".md")):
                text[f"{sub}/{n}"] = open(os.path.join(d, n), "rb").read()
    return text


def tar_of(files):
    """A GNU tar of {name: bytes}, every time 0"""
    buf = io.BytesIO()
    with tarfile.open(fileobj=buf, mode="w", format=tarfile.GNU_FORMAT) as t:
        for name, data in files.items():
            info = tarfile.TarInfo(name)
            info.size, info.mtime = len(data), 0
            t.addfile(info, io.BytesIO(data))
    return buf.getvalue()


def habit_archives(work):
    """.tar.gz files whose blocks follow their encoder's habit: {label: (path, files)}. The text is
    this repository's source, three times over where it must make enough blocks to show a habit
    (each copy lies farther back than Deflate's window reaches, so it compresses as the first)."""
    text = repo_text()
    made = {}
    # a .gz inside: its compressed bytes go into the outer stream's stored blocks, whose headers
    # break it up; its own block headers are not the outer stream's (blocks of 127 symbols, zlib's
    # with memLevel 1: several fit in one stored block, so their headers are found)
    c = zlib.compressobj(6, zlib.DEFLATED, 31, 1)
    files = {**{f"a/{k}": v for k, v in text.items()}, "inner.txt.gz": c.compress(b"".join(text.values())) + c.flush(),
             **{f"b/{k}": v for k, v in text.items()}}
    made["with a .gz inside (zlib)"] = (gzip.compress(tar_of(files), 6, mtime=0), files)
    # pigz: zlib on 128 KiB chunks, each ended by a sync marker (an empty stored block) or, when
    # the block already ends on a byte boundary, by nothing
    files = {f"copy{c}/{k}": v for c in range(3) for k, v in text.items()}
    tar = tar_of(files)
    c = zlib.compressobj(6, zlib.DEFLATED, 31)
    chunks = [tar[i:i + 131072] for i in range(0, len(tar), 131072)]
    z = b"".join(c.compress(ch) + c.flush(zlib.Z_FINISH if i + 1 == len(chunks) else zlib.Z_BLOCK if i % 8 == 7 else zlib.Z_SYNC_FLUSH)
                 for i, ch in enumerate(chunks))
    made["in 128 KiB chunks (as pigz)"] = (z, files)
    gz = shutil.which("gzip") or next((p for p in (r"C:\Program Files\Git\usr\bin\gzip.exe",) if os.path.isfile(p)), None)
    if gz:  # GNU gzip ends some blocks at a multiple of 4096 symbols
        made["from GNU gzip"] = (subprocess.run([gz, "-6", "-n", "-c"], input=tar, capture_output=True, check=True).stdout, files)
    else:
        SKIPPED.append("rescue of a GNU gzip tar.gz: gzip not found")
    out = {}
    for k, (label, (data, files)) in enumerate(made.items()):
        path = os.path.join(work, f"habit{k}.tar.gz")
        open(path, "wb").write(data)
        out[label] = (path, files)
    return out


def device_path(name):
    """Where gpuzip writes an entry of this name: with a _ before each part this Windows takes for
    a device (GetFullPathNameW makes \\\\.\\NAME of it, and opens nothing); elsewhere as it is"""
    if os.name != "nt":
        return name
    import ctypes
    buf = ctypes.create_unicode_buffer(512)

    def device(part):
        ctypes.windll.kernel32.GetFullPathNameW("C:\\gpuzip\\" + part, 512, buf, None)
        return buf.value.startswith("\\\\.\\")
    return "/".join("_" + p if device(p) else p for p in name.split("/"))


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
    gzips = build_gzips(src, work)
    habits = habit_archives(work)
    failures = 0
    gpu = has_gpu()
    modes = ("cpu", "gpu", "gpu-batches", "auto") if gpu else ("cpu", "auto")
    if not gpu:
        SKIPPED.append("gpu modes: no GPU that gpuzip can use (auto mode still runs, on the CPU)")
    print(f"testing: {' '.join(GPUZIP)}")
    print(f"source: {len(expected)} files and empty folders")
    for why in SKIPPED:
        print(f"skip  {why}")
    print()
    cases = [(label, path, expected) for label, path in archives.items()]
    cases += [(label, path, exp) for label, (path, exp) in gzips.items()]
    for label, path, exp in cases:
        # .NET Framework stores times its own way; a plain .gz output takes the .gz file's time
        check_times = "PowerShell" not in label and not path.endswith(".txt.gz")
        for mode in modes:
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

    # a large stream that hardly compresses fills every segment's input (64 MB), so segments start
    # at unaligned bytes: the GPU decoder once ran past its input buffer there and every such file
    # fell back to the CPU. It must decode on the GPU, every byte right (Rust, with a GPU).
    if gpu:
        rnd = random.Random(5)
        words = b" ".join(rnd.choice([b"block", b"stream", b"segment", b"header"]) for _ in range(300))[:1024]
        raw = b"".join(rnd.randbytes(3072) + words for _ in range(60000))  # 246 MB, compresses to about 0.8
        hard = os.path.join(work, "hard.bin.gz")
        with open(hard, "wb") as f:
            f.write(gzip.compress(raw, 1))
        rc, rep = run_gpuzip("t", hard, "--mode", "gpu", "-q")
        routes = rep.get("routes", {})
        ok = rc == 0 and routes.get("gpu", {}).get("files") == 1 and not routes.get("cpu", {}).get("files") and not rep.get("notes")
        failures += not ok
        print(f"{'ok  ' if ok else 'FAIL'} a 246 MB gzip stream that hardly compresses, on the GPU: "
              f"routes {routes}, notes {rep.get('notes')}")
        os.remove(hard)

    # a damaged gzip file must fail, whichever decodes it
    path, _ = gzips["gzip, one member (Python gzip)"]
    data = bytearray(open(path, "rb").read())
    data[len(data) // 2] ^= 0x55
    bad = os.path.join(work, "damaged.txt.gz")
    open(bad, "wb").write(bytes(data))
    for mode in [m for m in modes if m != "gpu-batches"]:
        rc, rep = run_gpuzip("t", bad, "--mode", mode, "-q")
        ok = rc == 2 and rep.get("failed")
        failures += not ok
        print(f"{'ok  ' if ok else 'FAIL'} damaged gzip refused{'':<26} {mode:<5} {(rep.get('failed') or [{}])[0].get('reason', rep)}")

    # rescue: a .tar.gz with a zeroed stretch must give back nearly every file, the
    # damage must be reported, and no file the report calls intact may differ from the source.
    # Two stretches in the random blobs (stored blocks) both decode in step: the CRC fails and
    # cannot say where, so blaming the block with the first one would vouch for the other
    src_tgz = archives["tar.gz (Python tarfile)"]
    files = {k: v for k, v in expected.items() if v[0] != "dir"}
    for where in ((0.4,), (0.85,), (0.25, 0.4)):  # the random blobs, then text, then two in the blobs
        data = bytearray(open(src_tgz, "rb").read())
        for w in where:
            p = int(len(data) * w)
            data[p:p + 2048] = bytes(2048)
        label = " and ".join(f"{int(w * 100)}%" for w in where)
        bad = os.path.join(work, f"damaged{int(where[0] * 100)}.tar.gz")
        open(bad, "wb").write(bytes(data))
        out = os.path.join(work, f"rescued{int(where[0] * 100)}")
        shutil.rmtree(out, ignore_errors=True)
        r = subprocess.run([*GPUZIP, "rescue", bad, "-o", out, "--json"], capture_output=True, text=True, encoding="utf-8")
        try:
            rep = json.loads(r.stdout.strip().splitlines()[-1])
        except (json.JSONDecodeError, IndexError):
            rep = {}
        got = tree(out)
        identical = sum(1 for k, v in files.items() if k in got and got[k][0] == v[0])
        t = rep.get("tar") or {}
        intact, unverified = t.get("intact", -1), t.get("unverified", 0)
        # the test's text repeats a small vocabulary, so a damaged block taints what copies from
        # it for a long way: many files come back partial, and that is the honest answer
        reported = rep.get("damaged_blocks", 0) >= 1 or rep.get("crc_check") == 0
        # files called intact: every file not listed otherwise, unless all are unverifiable
        flagged = set(t.get("inferred", [])) | set(t.get("suspect", [])) | set(t.get("partial", []))
        intact_wrong = [] if unverified else [k for k, v in files.items() if k in got and got[k][0] != v[0] and k not in flagged]
        ok = r.returncode == 2 and reported and identical >= 0.5 * len(files) and identical >= intact and not intact_wrong
        failures += not ok
        print(f"{'ok  ' if ok else 'FAIL'} rescue of a tar.gz zeroed at {label}: {identical} of {len(files)} files identical, "
              f"{intact} called intact, {unverified} unverifiable, {len(t.get('partial', []))} partial, "
              f"{rep.get('damaged_blocks', '?')} damaged blocks, exit {r.returncode}"
              + (f"; CALLED INTACT BUT WRONG: {intact_wrong[:3]}" if intact_wrong else ""))

    # The rescue holds the first part of the output in memory and decodes the rest again to write
    # it out (GR_HOLD: how many bytes it holds). Holding none of it, or 64 KB, must give the same
    # files and the same report, inferred bytes and their model included.
    bad = os.path.join(work, "damaged85.tar.gz")
    runs = {}
    for hold in (None, "0", "65536"):
        out = os.path.join(work, f"rescued_hold{hold}")
        shutil.rmtree(out, ignore_errors=True)
        env = dict(os.environ)
        if hold is not None:
            env["GR_HOLD"] = hold
        r = subprocess.run([*GPUZIP, "rescue", bad, "-o", out, "--json"], capture_output=True, text=True, encoding="utf-8", env=env)
        try:
            rep = json.loads(r.stdout.strip().splitlines()[-1])
        except (json.JSONDecodeError, IndexError):
            rep = {}
        rep.pop("seconds", None)
        runs[hold] = (r.returncode, rep, tree(out))
    for hold in ("0", "65536"):
        ok = runs[hold] == runs[None] and runs[None][1].get("damaged_blocks", 0) >= 1
        failures += not ok
        print(f"{'ok  ' if ok else 'FAIL'} rescue holding {hold} bytes of the output: the same files and report as holding it all")

    # A decompression bomb: the rescue stops at --max-output, says so and leaves no file; with no
    # limit it gives back every byte
    bomb = os.path.join(work, "zeros.gz")
    with gzip.open(bomb, "wb", compresslevel=9) as g:
        for _ in range(64):
            g.write(bytes(1 << 20))
    out = os.path.join(work, "rescued_bomb")
    shutil.rmtree(out, ignore_errors=True)
    r = subprocess.run([*GPUZIP, "rescue", bomb, "-o", out, "--max-output", "1M"], capture_output=True, text=True, encoding="utf-8")
    left = os.listdir(out) if os.path.isdir(out) else []
    ok = r.returncode == 1 and "--max-output" in r.stderr and not left
    failures += not ok
    print(f"{'ok  ' if ok else 'FAIL'} rescue of a gzip bomb stopped at --max-output 1M: exit {r.returncode}, {len(left)} files left")
    r = subprocess.run([*GPUZIP, "rescue", bomb, "-o", out, "--max-output", "0", "--json"], capture_output=True, text=True, encoding="utf-8")
    got = os.path.join(out, "zeros")
    ok = r.returncode == 0 and os.path.isfile(got) and os.path.getsize(got) == 64 << 20
    failures += not ok
    print(f"{'ok  ' if ok else 'FAIL'} rescue of the same with --max-output 0: exit {r.returncode}, every byte back")

    # rescue of tar.gz files whose blocks follow their encoder's habit. Undamaged, each
    # must come back whole with no block flagged: a .gz inside one (its block headers lie in the
    # outer stream's stored blocks), pigz's short blocks at the end of its chunks, GNU gzip's at a
    # multiple of 4096 symbols. Damaged (zeros, flipped bits, every CR LF made LF, zeros inside
    # the .gz inside), no file called intact may differ, and the damage must be reported: damage
    # that can keep the decoder in step (flipped bits, lost CRs) leaves no file vouched for.
    for label, (path, files) in habits.items():
        data = open(path, "rb").read()
        mid = len(data) // 2
        flipped = bytearray(data)
        rng = random.Random(label)
        for _ in range(8):
            flipped[rng.randrange(len(flipped))] ^= 1 << rng.randrange(8)
        damages = [("undamaged", data), ("2 KB zeroed", data[:mid] + bytes(2048) + data[mid + 2048:]),
                   ("8 bits flipped", bytes(flipped)), ("CR LF made LF", data.replace(b"\r\n", b"\n"))]
        if "inner.txt.gz" in files:  # in the stored data, where only the stream inside can tell
            inner = files["inner.txt.gz"]
            at = next((a for f in (0.5, 0.4, 0.6, 0.3, 0.7) if (a := data.find(inner[int(len(inner) * f):][:64])) > 0), -1)
            if at > 0:
                damages.append(("1 KB zeroed in the .gz inside", data[:at] + bytes(1024) + data[at + 1024:]))
            else:
                print("skip  rescue of a tar.gz with zeros in a .gz inside: the encoder did not store it as it is")
        for how, bad_data in damages:
            bad = os.path.join(work, "habit_damaged.tar.gz")
            open(bad, "wb").write(bad_data)
            out = os.path.join(work, "rescued_habit")
            shutil.rmtree(out, ignore_errors=True)
            r = subprocess.run([*GPUZIP, "rescue", bad, "-o", out, "--json"], capture_output=True, text=True, encoding="utf-8")
            try:
                rep = json.loads(r.stdout.strip().splitlines()[-1])
            except (json.JSONDecodeError, IndexError):
                rep = {}
            got = {k: v[0] for k, v in tree(out).items()}
            want = {k: hashlib.sha256(v).hexdigest() for k, v in files.items()}
            identical = sum(1 for k, v in want.items() if got.get(k) == v)
            t = rep.get("tar") or {}
            intact, unverified = t.get("intact", -1), t.get("unverified", 0)
            flagged = set(t.get("inferred", [])) | set(t.get("suspect", [])) | set(t.get("partial", []))
            intact_wrong = [] if unverified else [k for k, v in want.items() if k in got and got[k] != v and k not in flagged]
            if how == "undamaged":
                ok = r.returncode == 0 and rep.get("damaged_blocks") == 0 and intact == len(files) == identical
            elif bad_data == data:
                continue  # no CR LF in it
            else:
                reported = rep.get("damaged_blocks", 0) >= 1 or rep.get("crc_check") == 0
                ok = r.returncode == 2 and reported and not intact_wrong
                if "inside" in how:  # stored data: only the CRC tells, and then no file is vouched for
                    ok = ok and ("inner.txt.gz" in flagged or unverified > 0)
            failures += not ok
            print(f"{'ok  ' if ok else 'FAIL'} rescue of a tar.gz {label}, {how}: {identical} of {len(files)} identical, "
                  f"{intact} called intact, {unverified} unverifiable, {rep.get('damaged_blocks', '?')} damaged blocks, exit {r.returncode}"
                  + (f"; CALLED INTACT BUT WRONG: {intact_wrong[:3]}" if intact_wrong else ""))

    # the same .gz inside, undamaged, at 120 places in the tar: whether its block headers, found in
    # the outer stream's stored blocks, were taken for the stream's own hung on where the blocks
    # fell (the archive above failed so on CI after an edit to this file changed its text; before
    # the fix, 8 of 200 such places flagged with zlib, 15 with zlib-ng; after it, 1 of 120 with
    # zlib-ng, a header found in a stored block's length field). None may be: 120 places miss
    # that regression less than once in a hundred runs, in about 50 s
    text = repo_text()
    c = zlib.compressobj(6, zlib.DEFLATED, 31, 1)
    inner = c.compress(b"".join(text.values())) + c.flush()
    flagged_at = []
    for v in range(120):
        files = {"pad.bin": random.Random(v).randbytes(v * 1531), **{f"a/{k}": d for k, d in text.items()},
                 "inner.txt.gz": inner, **{f"b/{k}": d for k, d in text.items()}}
        p = os.path.join(work, "shifted.tar.gz")
        open(p, "wb").write(gzip.compress(tar_of(files), 6, mtime=0))
        out = os.path.join(work, "rescued_shifted")
        shutil.rmtree(out, ignore_errors=True)
        r = subprocess.run([*GPUZIP, "rescue", p, "-o", out, "--json"], capture_output=True, text=True, encoding="utf-8")
        try:
            rep = json.loads(r.stdout.strip().splitlines()[-1])
        except (json.JSONDecodeError, IndexError):
            rep = {}
        if r.returncode != 0 or rep.get("damaged_blocks") != 0 or (rep.get("tar") or {}).get("intact") != len(files):
            flagged_at.append(v)
    failures += bool(flagged_at)
    print(f"{'ok  ' if not flagged_at else 'FAIL'} rescue of 120 undamaged tar.gz files with a .gz inside at different places: "
          f"{120 - len(flagged_at)} clean" + (f"; FLAGGED AS DAMAGED: variants {flagged_at[:5]}" if flagged_at else ""))

    # rescue of gzip files of one Deflate block: a small text, and repetitive data that
    # packs megabytes into one block, with zeros written over the middle. Zeros decode as valid
    # codes: the decoder may fall back in step after them (only the CRC tells), or run on to the
    # end of the file. No byte called exact may differ from the original at its place, and the
    # bytes before the damage must be certified about as far as gzip decodes them right. Exact
    # bytes are told apart from unknown ones by rescuing with two fill bytes, from suspect ones by
    # the damaged list.
    rust_src = os.path.join(HERE, "..", "rust", "src")
    names = ["ziprescue.rs"] + sorted(n for n in os.listdir(rust_src) if n.endswith(".rs") and n != "ziprescue.rs")
    code = b"".join(open(os.path.join(rust_src, n), "rb").read() for n in names)
    repeated = b"gpuzip release check " * 200000
    cases = []
    for n in (20000, 60000):
        for zeros in (512, 64):
            cases.append((f"{n // 1000} KB of code, {zeros} zero bytes", code[:n], 6, zeros))
    for level in (6, 9):  # level 6: the decoder falls back in step; level 9: it runs to the end
        cases.append((f"repetitive 4.2 MB (level {level}), 4096 zero bytes", repeated, level, 4096))
    for label, plain, level, zeros in cases:
        data = bytearray(gzip.compress(plain, level, mtime=0))
        mid = len(data) // 2
        data[mid:mid + zeros] = bytes(zeros)
        bad = os.path.join(work, "single_block.gz")
        open(bad, "wb").write(bytes(data))
        d = zlib.decompressobj(31)
        try:
            got = d.decompress(bytes(data))
        except zlib.error:
            got = bytearray()
            d = zlib.decompressobj(31)
            for i in range(0, len(data), 64):  # what gzip gives before it stops
                try:
                    got += d.decompress(bytes(data[i:i + 64]))
                except zlib.error:
                    break
        gzip_right = next((i for i in range(min(len(got), len(plain))) if got[i] != plain[i]), min(len(got), len(plain)))
        runs = []
        for fill in (0, 255):
            out = os.path.join(work, f"rescued_single{fill}")
            shutil.rmtree(out, ignore_errors=True)
            r = subprocess.run([*GPUZIP, "rescue", bad, "-o", out, "--fill", str(fill), "--json"], capture_output=True, text=True, encoding="utf-8")
            try:
                rep = json.loads(r.stdout.strip().splitlines()[-1])
                runs.append((open(rep["output"], "rb").read(), rep, r.returncode))
            except (json.JSONDecodeError, IndexError, KeyError, TypeError, OSError):
                runs.append((b"", {}, r.returncode))
        (a, rep, rc), (b, _, _) = runs
        flagged = bytearray(len(a))
        for dmg in rep.get("damaged", []):
            s = dmg["output_offset"]
            e = min(len(flagged), s + dmg["length"])
            flagged[s:e] = b"\1" * max(0, e - s)
        exact = [i for i in range(min(len(a), len(b))) if not flagged[i] and a[i] == b[i]]
        wrong = sum(1 for i in exact if i >= len(plain) or a[i] != plain[i])
        certified = next((k for k, i in enumerate(exact) if i != k or a[i] != plain[i]), len(exact))
        ok = (rc == 2 and a and wrong == 0 and rep.get("damaged_blocks", 0) >= 1 and rep.get("inferred_bytes") == 0
              and rep.get("exact_bytes") == len(exact) and certified >= 0.95 * gzip_right)
        failures += not ok
        print(f"{'ok  ' if ok else 'FAIL'} rescue of one block, {label}: {certified:,} bytes certified before the damage "
              f"(gzip decodes {gzip_right:,} right), {len(exact):,} called exact, {wrong:,} of them wrong, "
              f"{rep.get('damaged_blocks', '?')} damaged block, exit {rc}")

    # rescue of ZIPs: without its central directory (cut, or zeroed), and with a zeroed
    # stretch inside; every entry called intact must be identical, the damage reported
    src_zip = archives["Python zipfile level 9"]
    data = open(src_zip, "rb").read()
    files = {k: v for k, v in expected.items() if v[0] != "dir"}
    cut = int(len(data) * 0.7)
    with zipfile.ZipFile(src_zip) as z:  # entries whole before the cut: what a cut file can give back
        before_cut = sum(1 for i in z.infolist() if not i.is_dir()
                         and i.header_offset + 30 + len(i.filename.encode()) + len(i.extra) + i.compress_size <= cut)
    cases = {
        "cut at 70%": (data[:cut], before_cut),
        "directory zeroed": (data[: int(len(data) * 0.97)] + bytes(len(data) - int(len(data) * 0.97)), int(0.6 * len(files))),
        "4 KB zeroed inside": (data[: len(data) // 2] + bytes(4096) + data[len(data) // 2 + 4096:], int(0.6 * len(files))),
    }
    for label, (damaged, expect_intact) in cases.items():
        bad = os.path.join(work, "damaged.zip")
        open(bad, "wb").write(damaged)
        out = os.path.join(work, "rescued_zip")
        shutil.rmtree(out, ignore_errors=True)
        r = subprocess.run([*GPUZIP, "rescue", bad, "-o", out, "--json"], capture_output=True, text=True, encoding="utf-8")
        try:
            rep = json.loads(r.stdout.strip().splitlines()[-1])
        except (json.JSONDecodeError, IndexError):
            rep = {}
        got = tree(out)
        status = {e["name"]: e["status"] for e in rep.get("entries", [])}
        identical = sum(1 for k, v in files.items() if k in got and got[k][0] == v[0])
        intact_wrong = [k for k, v in files.items() if status.get(k) == "intact" and not (k in got and got[k][0] == v[0])]
        counts = rep.get("counts", {})
        ok = r.returncode == 2 and not intact_wrong and identical >= expect_intact and counts.get("intact", 0) >= expect_intact
        failures += not ok
        print(f"{'ok  ' if ok else 'FAIL'} rescue of a zip, {label}: {identical} of {len(files)} identical, "
              f"{counts.get('intact', '?')} intact, {counts.get('partial', '?')} partial, {counts.get('unverified', '?')} unverified, "
              f"directory {'read' if rep.get('directory_ok') else 'lost'}, exit {r.returncode}"
              + (f"; CALLED INTACT BUT WRONG: {intact_wrong[:3]}" if intact_wrong else ""))

    # an entry whose local header is destroyed, with the 28-byte extra field Info-ZIP writes:
    # its data is looked for at several places after where the header was, and the one where
    # the stream begins, or else whose data ends where the next header begins, must win; from
    # the wrong one the output is cut short and out of place. How much the damaged block costs
    # depends on the zlib that made the archive: zlib-ng (Python 3.14 on Windows) ends blocks
    # sooner than zlib (Linux, macOS), and the right start gives back 76-91% of the entry in
    # place, the wrong one 3-5%. Half is the bar.
    rnd = random.Random(5)
    words = ["".join(rnd.choice("abcdefghijklmnopqrstuvwxyz") for _ in range(rnd.randint(2, 9))) for _ in range(3000)]
    texts = {f"w{k}.txt": " ".join(rnd.choice(words) for _ in range(150000)).encode() for k in range(3)}
    extra = struct.pack("<HHB2I", 0x5455, 9, 3, 1700000000, 1700000000) + struct.pack("<HHBBIBI", 0x7875, 11, 1, 4, 1000, 4, 1000)
    z_path = os.path.join(work, "infozip.zip")
    with zipfile.ZipFile(z_path, "w", zipfile.ZIP_DEFLATED) as z:
        for n, t in texts.items():
            zi = zipfile.ZipInfo(n, (2024, 1, 1, 0, 0, 0))
            zi.compress_type, zi.extra = zipfile.ZIP_DEFLATED, extra
            z.writestr(zi, t)
    raw = open(z_path, "rb").read()
    with zipfile.ZipFile(z_path) as z:
        info = z.getinfo("w1.txt")
    h, d0 = info.header_offset, info.header_offset + 30 + len(info.filename) + len(extra)
    mid = d0 + info.compress_size // 2
    cases = {
        "its header zeroed and 4 KB inside its data": raw[:h] + bytes(30) + raw[h + 30:mid] + bytes(4096) + raw[mid + 4096:],
        "zeros over its header and the start of its data": raw[:h] + bytes(d0 + 3000 - h) + raw[d0 + 3000:],
        "other data over its header and the start of its data": raw[:h] + random.Random(1).randbytes(d0 + 3000 - h) + raw[d0 + 3000:],
    }
    for label, damaged in cases.items():
        bad = os.path.join(work, "damaged_infozip.zip")
        open(bad, "wb").write(damaged)
        out = os.path.join(work, "rescued_infozip")
        shutil.rmtree(out, ignore_errors=True)
        r = subprocess.run([*GPUZIP, "rescue", bad, "-o", out, "--json"], capture_output=True, text=True, encoding="utf-8")
        try:
            rep = json.loads(r.stdout.strip().splitlines()[-1])
        except (json.JSONDecodeError, IndexError):
            rep = {}
        status = {e["name"]: e["status"] for e in rep.get("entries", [])}
        p = os.path.join(out, "w1.txt")
        got = open(p, "rb").read() if os.path.exists(p) else b""
        orig = texts["w1.txt"]
        in_place = sum(1 for a, b in zip(got, orig) if a == b)
        others = all(status.get(n) == "intact" and open(os.path.join(out, n), "rb").read() == texts[n] for n in ("w0.txt", "w2.txt"))
        ok = status.get("w1.txt") == "partial" and len(got) == len(orig) and in_place >= 0.5 * len(orig) and others
        failures += not ok
        print(f"{'ok  ' if ok else 'FAIL'} rescue of an Info-ZIP entry with {label}: {in_place:,} of {len(orig):,} bytes "
              f"right in place, {status.get('w1.txt')}, the other entries intact {others}, exit {r.returncode}")

    # names that are not flagged UTF-8: CP437 from Windows tools, UTF-8 from Unix ones. The
    # directory decodes them; each entry's local header must be matched to it on the name's bytes,
    # not taken for destroyed, and an entry found only by scanning must keep its name
    def raw_zip(entries):
        """A ZIP by hand: [(name bytes, made by, data)], Deflate, no UTF-8 flag"""
        body, cd = b"", b""
        for name, made_by, data in entries:
            c = zlib.compressobj(6, zlib.DEFLATED, -15)
            comp = c.compress(data) + c.flush()
            crc, at = zlib.crc32(data), len(body)
            body += struct.pack("<IHHHHHIIIHH", 0x04034B50, 20, 0, 8, 0, 0x5821, crc, len(comp), len(data), len(name), 0) + name + comp
            cd += struct.pack("<IBBHHHHHIIIHHHHHII", 0x02014B50, 20, made_by, 20, 0, 8, 0, 0x5821, crc, len(comp), len(data),
                              len(name), 0, 0, 0, 0, 0, at) + name
        return body + cd + struct.pack("<IHHHHIIH", 0x06054B50, 0, 0, len(entries), len(entries), len(cd), len(body), 0)
    rnd = random.Random(9)
    filler = " ".join(rnd.choice(words) for _ in range(40000)).encode()
    named = [("data/a\xa4o 2026/notes.txt".encode("latin-1"), 0, b"notes " * 3000, "data/a\u00f1o 2026/notes.txt"),
             ("datos/a\u00f1o/x.txt".encode("utf-8"), 3, b"x " * 5000, "datos/a\u00f1o/x.txt"),
             (b"filler.txt", 0, filler, "filler.txt")]
    zdata = raw_zip([(n, m, d) for n, m, d, _ in named])
    fill_at = zdata.find(b"filler.txt") + len(b"filler.txt") + 2000  # inside filler.txt's data
    cd_at = zdata.find(struct.pack("<I", 0x02014B50))
    for label, damaged in (("the directory read, 4 KB zeroed in another entry", zdata[:fill_at] + bytes(4096) + zdata[fill_at + 4096:]),
                           ("the directory lost", zdata[:cd_at])):
        bad = os.path.join(work, "names.zip")
        open(bad, "wb").write(damaged)
        out = os.path.join(work, "rescued_names")
        shutil.rmtree(out, ignore_errors=True)
        r = subprocess.run([*GPUZIP, "rescue", bad, "-o", out, "--json"], capture_output=True, text=True, encoding="utf-8")
        try:
            rep = json.loads(r.stdout.strip().splitlines()[-1])
        except (json.JSONDecodeError, IndexError):
            rep = {}
        status = {e["name"]: (e["status"], e.get("detail", "")) for e in rep.get("entries", [])}
        back = [want for _, _, data, want in named[:2]
                if status.get(want, ("", ""))[0] == "intact" and "destroyed" not in status[want][1]
                and os.path.exists(os.path.join(out, want)) and open(os.path.join(out, want), "rb").read() == data]
        ok = len(back) == 2
        failures += not ok
        print(f"{'ok  ' if ok else 'FAIL'} rescue of a zip with names not flagged UTF-8, {label}: {len(back)} of 2 back under their names"
              + ("" if ok else f"; got {ascii(sorted(status))}, {ascii([v[1] for v in status.values()])}"))

    # names Windows may take for devices (NUL on every Windows; CON, AUX, COM1... on Windows 10):
    # what is written under one is lost, NUL's with no error. Each must come back, under its name
    # or, where this Windows takes a part of it for a device, with a _ before that part, said in a note
    dev_files = {"nul": b"nul data", "CON.txt": b"con data", "aux/x.txt": b"in aux", "com1": b"com1 data",
                 "Lpt¹.log": b"lpt1", "d/NUL/inside.txt": b"inside", "nul.tar.gz": b"a name only", "plain.txt": b"plain"}
    dev_zip = os.path.join(work, "devices.zip")
    with zipfile.ZipFile(dev_zip, "w", zipfile.ZIP_DEFLATED) as z:
        for n, d in dev_files.items():
            z.writestr(n, d)
    dev_tgz = os.path.join(work, "devices.tar.gz")
    open(dev_tgz, "wb").write(gzip.compress(tar_of(dev_files), 6, mtime=0))
    dev_gz = os.path.join(work, "nul.gz")
    open(dev_gz, "wb").write(gzip.compress(b"one file named nul", 6, mtime=0))
    for cmd in ("x", "rescue"):
        for label, path, want in (("zip", dev_zip, dev_files), ("tar.gz", dev_tgz, dev_files), (".gz named nul", dev_gz, {"nul": b"one file named nul"})):
            out = os.path.join(work, "devices_out")
            shutil.rmtree(out, ignore_errors=True)
            r = subprocess.run([*GPUZIP, cmd, path, "-o", out, "--json"], capture_output=True, text=True, encoding="utf-8")
            try:
                rep = json.loads(r.stdout.strip().splitlines()[-1])
            except (json.JSONDecodeError, IndexError):
                rep = {}
            written = {k: open(os.path.join(out, k), "rb").read() for k in tree(out) if not k.endswith("/")}
            expect = {device_path(n): d for n, d in want.items()}
            renamed = sorted(n for n in want if device_path(n) != n)
            noted = any("this Windows takes for" in n for n in rep.get("notes", []))
            ok = written == expect and noted == bool(renamed)
            failures += not ok
            print(f"{'ok  ' if ok else 'FAIL'} {cmd} of a {label} holding device names: {len(written)} of {len(expect)} files back, "
                  f"renamed here: {ascii(renamed)}" + ("" if ok else f"; wrote {ascii(sorted(written))}, notes {ascii(rep.get('notes'))}"))

    # entries whose bytes overlap, the way zip bombs multiply their data: one stream named by 20
    # entries, and Fifield's, where each entry's stream opens with a stored block quoting the next
    # one's local header (measured before: 103 KB written out as 2 GB, 12 KB as 200 MB). Each
    # entry is refused and nothing written; `t` fails them too
    def local_header(name, crc, csize, size):
        n = name.encode()
        return struct.pack("<IHHHHHIIIHH", 0x04034B50, 20, 0, 8, 0, 0x21, crc, csize, size, len(n), 0) + n

    def central_entry(name, crc, csize, size, at):
        n = name.encode()
        return struct.pack("<IHHHHHHIIIHHHHHII", 0x02014B50, 20, 20, 0, 8, 0, 0x21, crc, csize, size, len(n), 0, 0, 0, 0, 0, at) + n

    def zip_with(body, cds):
        cd = b"".join(cds)
        return body + cd + struct.pack("<IHHHHIIH", 0x06054B50, 0, 0, len(cds), len(cds), len(cd), len(body), 0)

    kernel = bytes(10 << 20)
    c = zlib.compressobj(9, zlib.DEFLATED, -15)
    kernel_z, kernel_crc = c.compress(kernel) + c.flush(), zlib.crc32(kernel)
    one_stream = zip_with(local_header("kernel.bin", kernel_crc, len(kernel_z), len(kernel)) + kernel_z,
                          [central_entry(f"copy{i:02}.bin", kernel_crc, len(kernel_z), len(kernel), 0) for i in range(20)])
    stream, content, chain = kernel_z, kernel, []
    for i in reversed(range(20)):
        lh = local_header(f"f{i:02}.bin", zlib.crc32(content), len(stream), len(content))
        chain.insert(0, (f"f{i:02}.bin", lh, zlib.crc32(content), len(stream), len(content)))
        stream = b"\x00" + struct.pack("<HH", len(lh), len(lh) ^ 0xFFFF) + lh + stream
        content = lh + content
    cds, at = [], 0
    for name, lh, crc, csize, size in chain:
        cds.append(central_entry(name, crc, csize, size, at))
        at += len(lh) + 5
    quoting = zip_with(chain[0][1] + stream[5 + len(chain[0][1]):], cds)
    for label, bomb in (("one stream named by 20 entries", one_stream), ("20 entries each quoting the next (Fifield)", quoting)):
        p = os.path.join(work, "overlap.zip")
        open(p, "wb").write(bomb)
        out = os.path.join(work, "overlap_out")
        shutil.rmtree(out, ignore_errors=True)
        results = {}
        for cmd in ("t", "x"):
            r = subprocess.run([*GPUZIP, cmd, p, *(["-o", out] if cmd == "x" else []), "--json"], capture_output=True, text=True, encoding="utf-8")
            try:
                rep = json.loads(r.stdout.strip().splitlines()[-1])
            except (json.JSONDecodeError, IndexError):
                rep = {}
            refused = sum(1 for f in rep.get("failed", []) if "overlap" in str(f))
            results[cmd] = (r.returncode, refused)
        written = sum(os.path.getsize(os.path.join(d, f)) for d, _, fs in os.walk(out) for f in fs) if os.path.isdir(out) else 0
        ok = results["t"] == (2, 20) and results["x"] == (2, 20) and written == 0
        failures += not ok
        print(f"{'ok  ' if ok else 'FAIL'} a ZIP of {label} ({len(bomb):,} bytes): t and x refuse "
              f"{results['t'][1]} and {results['x'][1]} of 20 entries (exit {results['t'][0]}, {results['x'][0]}), {written:,} bytes written")
        # rescue gives the stream back once, under the first name, and writes none of the rest
        # (measured before: the 2 GB and the 200 MB written)
        shutil.rmtree(out, ignore_errors=True)
        r = subprocess.run([*GPUZIP, "rescue", p, "-o", out, "--json"], capture_output=True, text=True, encoding="utf-8")
        try:
            rep = json.loads(r.stdout.strip().splitlines()[-1])
        except (json.JSONDecodeError, IndexError):
            rep = {}
        status = [e["status"] for e in rep.get("entries", [])]
        refused = sum(1 for e in rep.get("entries", []) if "no two entries share bytes" in e.get("detail", ""))
        files = [os.path.join(d, f) for d, _, fs in os.walk(out) for f in fs] if os.path.isdir(out) else []
        written = sum(os.path.getsize(f) for f in files)
        ok = r.returncode == 2 and status.count("intact") == 1 and refused == 19 and len(files) == 1 and written < 11 << 20
        failures += not ok
        print(f"{'ok  ' if ok else 'FAIL'} rescue of a ZIP of {label}: {status.count('intact')} intact, {refused} not written as "
              f"sharing its data, {len(files)} file of {written:,} bytes, exit {r.returncode}")
    shutil.rmtree(os.path.join(work, "overlap_out"), ignore_errors=True)

    # a tar's small files are written on several threads: order must hold as in a tar. A path
    # written twice keeps its last copy (the same path in another case too, where names ignore
    # it), small or large; a hard link made right after its target sees the target whole
    entries = [("dup.txt", b"first copy\n"), ("grow.bin", b"small first\n"), ("Case.txt", b"upper first\n")]
    entries += [(f"many/f{i:04}.txt", f"file {i}\n".encode() * 20) for i in range(2000)]
    entries += [("dup.txt", b"second copy, which must win\n"), ("grow.bin", bytes(range(256)) * 1500),
                ("case.txt", b"lower last\n"), ("target.txt", b"a hard link names me\n" * 50)]
    buf = io.BytesIO()
    with tarfile.open(fileobj=buf, mode="w", format=tarfile.GNU_FORMAT) as t:
        for name, data in entries:
            info = tarfile.TarInfo(name)
            info.size, info.mtime = len(data), 1700000000
            t.addfile(info, io.BytesIO(data))
        link = tarfile.TarInfo("hard.txt")
        link.type, link.linkname, link.mtime = tarfile.LNKTYPE, "target.txt", 1700000000
        t.addfile(link)
    order_tgz = os.path.join(work, "order.tar.gz")
    open(order_tgz, "wb").write(gzip.compress(buf.getvalue(), 6, mtime=0))
    out = os.path.join(work, "order_out")
    shutil.rmtree(out, ignore_errors=True)
    r = subprocess.run([*GPUZIP, "x", order_tgz, "-o", out], capture_output=True, text=True)
    want = {name: data for name, data in entries}  # the last copy of each
    if os.name == "nt" or sys.platform == "darwin":
        want.pop("Case.txt")
        case_file = next((n for n in os.listdir(out) if n.lower() == "case.txt"), "case.txt")
        want[case_file] = b"lower last\n"
    want["hard.txt"] = want["target.txt"]
    wrong = [n for n, d in want.items() if not os.path.isfile(os.path.join(out, n)) or open(os.path.join(out, n), "rb").read() != d]
    ok = r.returncode == 0 and not wrong
    failures += not ok
    print(f"{'ok  ' if ok else 'FAIL'} x of a tar.gz whose paths repeat, with a hard link: {len(want) - len(wrong)} of {len(want)} files right, "
          f"exit {r.returncode}" + (f"; WRONG: {wrong[:5]} {r.stderr.strip()[-200:]}" if wrong else ""))
    shutil.rmtree(out, ignore_errors=True)

    # a ZIP made on Unix keeps each file's permissions, the executable bit among them: restored
    # on Unix and macOS. Not from a ZIP made on DOS, whose high attribute bits mean nothing, and
    # never setuid
    modes = os.path.join(work, "modes.zip")
    with zipfile.ZipFile(modes, "w", zipfile.ZIP_DEFLATED) as z:
        for name, made_by, mode, data in (("run.sh", 3, 0o100755, b"#!/bin/sh\necho hi\n"), ("data.txt", 3, 0o100644, b"data\n"),
                                          ("suid", 3, 0o104750, b"no setuid\n"), ("dos.txt", 0, 0o100700, b"made on DOS\n")):
            info = zipfile.ZipInfo(name, (2026, 1, 2, 3, 4, 6))
            info.create_system, info.external_attr, info.compress_type = made_by, mode << 16, zipfile.ZIP_DEFLATED
            z.writestr(info, data)
    out = os.path.join(work, "modes_out")
    for cmd in ("x", "rescue"):
        shutil.rmtree(out, ignore_errors=True)
        r = subprocess.run([*GPUZIP, cmd, modes, "-o", out], capture_output=True, text=True)
        got = {n: os.stat(os.path.join(out, n)).st_mode & 0o7777 for n in ("run.sh", "data.txt", "suid", "dos.txt") if os.path.exists(os.path.join(out, n))}
        if os.name == "nt":
            ok = r.returncode == 0 and len(got) == 4  # no such bits on Windows: the files are there
        else:
            umask = os.umask(0)
            os.umask(umask)
            ok = r.returncode == 0 and got.get("run.sh") == 0o755 and got.get("data.txt") == 0o644 and got.get("suid") == 0o750 \
                and got.get("dos.txt") == 0o666 & ~umask
        failures += not ok
        print(f"{'ok  ' if ok else 'FAIL'} {cmd} of a ZIP made on Unix restores its modes: {ascii({n: oct(m) for n, m in got.items()})}"
              + (" (Windows: not checked)" if os.name == "nt" else ""))
    shutil.rmtree(out, ignore_errors=True)

    # tar names that differ only in case are one file where names ignore case (Windows, macOS):
    # the last copy is kept, and a note says so (linux-6.6.tar.gz has 13 such pairs); elsewhere
    # both are written
    cases = [("Makefile", b"upper\n"), ("makefile", b"lower, last\n"), ("dir/Read.me", b"first\n"), ("dir/READ.ME", b"second, last\n")]
    buf = io.BytesIO()
    with tarfile.open(fileobj=buf, mode="w", format=tarfile.GNU_FORMAT) as t:
        for name, data in cases:
            info = tarfile.TarInfo(name)
            info.size, info.mtime = len(data), 1700000000
            t.addfile(info, io.BytesIO(data))
    case_tgz = os.path.join(work, "case.tar.gz")
    open(case_tgz, "wb").write(gzip.compress(buf.getvalue(), 6, mtime=0))
    out = os.path.join(work, "case_out")
    for cmd in ("x", "rescue"):
        shutil.rmtree(out, ignore_errors=True)
        r = subprocess.run([*GPUZIP, cmd, case_tgz, "-o", out, "--json"], capture_output=True, text=True)
        try:
            notes = json.loads(r.stdout.strip().splitlines()[-1]).get("notes", [])
        except (json.JSONDecodeError, IndexError):
            notes = []
        noted = any("differ" in n and "only in case" in n for n in notes)
        on_disk = {os.path.relpath(os.path.join(d, f), out).replace("\\", "/"): open(os.path.join(d, f), "rb").read() for d, _, fs in os.walk(out) for f in fs}
        if os.name == "nt" or sys.platform == "darwin":
            lowered = {k.lower(): v for k, v in on_disk.items()}
            ok = r.returncode == 0 and noted and len(on_disk) == 2 and lowered.get("makefile") == b"lower, last\n" \
                and lowered.get("dir/read.me") == b"second, last\n"
        else:
            ok = r.returncode == 0 and not noted and on_disk == dict(cases)
        failures += not ok
        print(f"{'ok  ' if ok else 'FAIL'} {cmd} of a tar.gz with names that differ only in case: {len(on_disk)} files, noted: {noted}"
              + ("" if ok else f"; {ascii(sorted(on_disk))} {ascii(notes)}"))
    shutil.rmtree(out, ignore_errors=True)

    # a tar's symbolic links: made where the system lets (Unix); on Windows, which lets only an
    # administrator or Developer Mode make them, once what they name is written, as hard links
    # to files and copies of folders, with a note. Either way what a link names reads through it
    # (before: "failed to unpack" on Windows, exit 2). One leading out, or naming nothing, is not made
    buf = io.BytesIO()
    with tarfile.open(fileobj=buf, mode="w", format=tarfile.GNU_FORMAT) as t:
        for name, kind, payload in (("lib", "dir", ""), ("lib/real.txt", "file", b"the real file\n"), ("lib/alias.txt", "sym", "real.txt"),
                                    ("inc", "sym", "lib"), ("a/b/up", "sym", "../../lib"), ("alias2", "sym", "lib/alias.txt"),
                                    ("early", "sym", "later.txt"), ("later.txt", "file", b"named before it came\n"),
                                    ("dangling", "sym", "nope.txt"), ("out", "sym", "../../x")):
            info = tarfile.TarInfo(name)
            info.mtime = 1700000000 - (86400 if name == "lib" else 0)
            if kind == "dir":
                info.type, info.mode = tarfile.DIRTYPE, 0o755  # (tarfile's 644 is given too, last: not to be entered)
                t.addfile(info)
            elif kind == "sym":
                info.type, info.linkname = tarfile.SYMTYPE, payload
                t.addfile(info)
            else:
                info.size = len(payload)
                t.addfile(info, io.BytesIO(payload))
    links_tgz = os.path.join(work, "links.tar.gz")
    open(links_tgz, "wb").write(gzip.compress(buf.getvalue(), 6, mtime=0))
    out = os.path.join(work, "links_out")
    shutil.rmtree(out, ignore_errors=True)
    r = subprocess.run([*GPUZIP, "x", links_tgz, "-o", out, "--json"], capture_output=True, text=True, encoding="utf-8")
    try:
        notes = json.loads(r.stdout.strip().splitlines()[-1]).get("notes", [])
    except (json.JSONDecodeError, IndexError):
        notes = []
    reads = {p: open(os.path.join(out, p), "rb").read() if os.path.isfile(os.path.join(out, p)) else None
             for p in ("lib/alias.txt", "inc/real.txt", "a/b/up/real.txt", "alias2", "early")}
    real, later = b"the real file\n", b"named before it came\n"
    ok = r.returncode == 0 and reads == {"lib/alias.txt": real, "inc/real.txt": real, "a/b/up/real.txt": real, "alias2": real, "early": later}
    if os.name == "nt":
        ok = ok and any("hard link" in n for n in notes) and any("leading out" in n and "out" in n for n in notes) \
            and not os.path.exists(os.path.join(out, "out"))
    lib_time = os.path.getmtime(os.path.join(out, "lib"))
    ok = ok and abs(lib_time - (1700000000 - 86400)) < 2  # a folder's time, given once its files are written
    failures += not ok
    print(f"{'ok  ' if ok else 'FAIL'} x of a tar.gz with symbolic links: {sum(v is not None for v in reads.values())} of 5 read through, "
          f"folder time {'kept' if abs(lib_time - (1700000000 - 86400)) < 2 else 'LOST'}, exit {r.returncode}"
          + ("" if ok else f"; {ascii(reads)} {ascii(notes)}"))
    shutil.rmtree(out, ignore_errors=True)

    # a ZIP's exact times: Info-ZIP's extended timestamp (UTC seconds) and NTFS's (100 ns), over
    # the DOS time (local, 2 s); a folder's time; and the DOS read-only attribute, which an
    # extraction into the same folder must still replace
    def ut_field(ts):
        return struct.pack("<HHBI", 0x5455, 5, 1, ts)

    def ntfs_field(ts):
        ft = int(ts * 10_000_000) + 116444736000000000
        return struct.pack("<HHIHHQQQ", 0x000a, 32, 0, 1, 24, ft, ft, ft)
    times_zip = os.path.join(work, "times.zip")
    with zipfile.ZipFile(times_zip, "w", zipfile.ZIP_DEFLATED) as z:
        for name, extra, attr, data in (("ut.txt", ut_field(1700000001), 0, b"ut\n"), ("ntfs.txt", ntfs_field(1700000000.5), 0, b"ntfs\n"),
                                        ("ro.txt", b"", 0x01, b"read only\n"), ("dir/", ut_field(1700000000 - 3600), 0x10, b""),
                                        ("dir/in.txt", b"", 0, b"in a folder\n")):
            info = zipfile.ZipInfo(name, (2020, 1, 1, 0, 0, 0))
            info.extra, info.external_attr, info.create_system = extra, attr, 0
            z.writestr(info, data)
    out = os.path.join(work, "times_out")
    shutil.rmtree(out, ignore_errors=True)
    rcs = [subprocess.run([*GPUZIP, "x", times_zip, "-o", out], capture_output=True, text=True).returncode for _ in range(2)]
    got = {n: os.path.getmtime(os.path.join(out, n)) for n in ("ut.txt", "ntfs.txt", "dir")}
    read_only = os.stat(os.path.join(out, "ro.txt")).st_mode & 0o222 == 0  # (root may write it anyway)
    ok = rcs == [0, 0] and abs(got["ut.txt"] - 1700000001) < 0.01 and abs(got["ntfs.txt"] - 1700000000.5) < 0.01 \
        and abs(got["dir"] - (1700000000 - 3600)) < 0.01 and read_only
    failures += not ok
    print(f"{'ok  ' if ok else 'FAIL'} x of a ZIP with exact times and a read-only file, twice into one folder: exits {rcs}, "
          f"times {ascii({n: round(t, 2) for n, t in got.items()})}, read-only {read_only}")
    os.chmod(os.path.join(out, "ro.txt"), 0o644)
    shutil.rmtree(out, ignore_errors=True)

    # an extraction killed while it writes a large file leaves no part of it under its name:
    # every file there must be whole (measured before: 184 MB of 400 left as big.bin in the
    # tar.gz, 344 of 400 from the .gz, 296 of 600 of the ZIP's entry). The ZIP's is past the
    # 512 MiB above which an entry is written as it is decoded
    class Pattern:
        """`size` bytes that compress, read in pieces"""
        block = b"".join(b"line %07d of a file that compresses well\n" % i for i in range(30000))[:1 << 20]

        def __init__(self, size):
            self.size, self.pos = size, 0

        def read(self, n=-1):
            n = self.size - self.pos if n < 0 else min(n, self.size - self.pos)
            out = bytearray()
            while len(out) < n:
                at = (self.pos + len(out)) % len(self.block)
                out += self.block[at:at + n - len(out)]
            self.pos += n
            return bytes(out)

    def pattern_digest(size):
        h, p = hashlib.sha256(), Pattern(size)
        while chunk := p.read(1 << 20):
            h.update(chunk)
        return h.hexdigest()

    big, huge = 300 << 20, 540 << 20
    small = hashlib.sha256(b"small\n").hexdigest()
    kill_tgz, kill_gz, kill_zip = (os.path.join(work, n) for n in ("kill.tar.gz", "big.bin.gz", "kill.zip"))
    with gzip.open(kill_tgz, "wb", compresslevel=1) as g, tarfile.open(fileobj=g, mode="w|") as t:
        for name, size, f in (("small.txt", 6, io.BytesIO(b"small\n")), ("big.bin", big, Pattern(big))):
            info = tarfile.TarInfo(name)
            info.size = size
            t.addfile(info, f)
    with gzip.open(kill_gz, "wb", compresslevel=1) as g:
        src = Pattern(big)
        while chunk := src.read(1 << 20):
            g.write(chunk)
    with zipfile.ZipFile(kill_zip, "w", zipfile.ZIP_DEFLATED, compresslevel=1) as z:
        z.writestr("small.txt", b"small\n")
        with z.open("huge.bin", "w", force_zip64=True) as f:
            src = Pattern(huge)
            while chunk := src.read(1 << 20):
                f.write(chunk)
    big_digest = pattern_digest(big)
    for label, path, files, target in (("tar.gz", kill_tgz, {"small.txt": small, "big.bin": big_digest}, "big.bin"),
                                       (".gz", kill_gz, {"big.bin": big_digest}, "big.bin"),
                                       ("zip", kill_zip, {"small.txt": small, "huge.bin": pattern_digest(huge)}, "huge.bin")):
        out = os.path.join(work, "killed_out")
        shutil.rmtree(out, ignore_errors=True)
        os.makedirs(out)
        proc = subprocess.Popen([*GPUZIP, "x", path, "-o", out, "--mode", "cpu"], stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
        t0 = time.time()
        while proc.poll() is None and time.time() - t0 < 120:
            if any(n.startswith(target) for n in os.listdir(out)):
                time.sleep(0.03)  # into writing it (an empty file left under its name fails too)
                break
            time.sleep(0.005)
        killed = proc.poll() is None
        proc.kill()
        proc.wait()
        left = sorted(os.listdir(out))
        wrong = []
        for n in left:
            if n.endswith(".gpuzip-part"):
                continue  # not a final name: what a kill leaves on Linux and macOS
            h = hashlib.sha256()
            with open(os.path.join(out, n), "rb") as f:
                while chunk := f.read(1 << 20):
                    h.update(chunk)
            if files.get(n) != h.hexdigest():
                wrong.append(f"{n}: {os.path.getsize(os.path.join(out, n)):,} bytes")
        failures += bool(wrong)
        print(f"{'ok  ' if not wrong else 'FAIL'} x of a {label} killed while writing {target} ({'it was' if killed else 'it had ended'}): "
              f"left {left}" + (f"; NOT WHOLE: {wrong}" if wrong else ""))
    shutil.rmtree(os.path.join(work, "killed_out"), ignore_errors=True)
    for p in (kill_tgz, kill_gz, kill_zip):
        os.remove(p)

    # a docx (a ZIP of XML parts) with zeros inside its document: the text comes back, the
    # repaired part is well-formed XML, and the rebuilt document opens
    import xml.etree.ElementTree as ET
    paragraphs = [f"Paragraph {i} of the test document, with some words to find again." for i in range(200)]
    body = "".join(f'<w:p><w:r><w:t xml:space="preserve">{p}</w:t></w:r></w:p>' for p in paragraphs)
    docx = os.path.join(work, "doc.docx")
    with zipfile.ZipFile(docx, "w", zipfile.ZIP_DEFLATED) as z:
        z.writestr("[Content_Types].xml", '<?xml version="1.0" encoding="UTF-8"?><Types xmlns="http://schemas.openxmlformats.org/package/2006/content-types">'
                   '<Default Extension="xml" ContentType="application/xml"/></Types>')
        z.writestr("_rels/.rels", '<?xml version="1.0" encoding="UTF-8"?><Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships"/>')
        z.writestr("word/document.xml", '<?xml version="1.0" encoding="UTF-8"?><w:document xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main">'
                   f'<w:body>{body}</w:body></w:document>')
    d = bytearray(open(docx, "rb").read())
    with zipfile.ZipFile(docx) as z:
        info = z.getinfo("word/document.xml")
        start = info.header_offset + 30 + len(info.filename.encode()) + len(info.extra)
        d[start + info.compress_size // 2:start + info.compress_size // 2 + 256] = bytes(256)
    bad = os.path.join(work, "damaged.docx")
    open(bad, "wb").write(bytes(d))
    out = os.path.join(work, "rescued_docx")
    r = subprocess.run([*GPUZIP, "rescue", bad, "-o", out, "--json"], capture_output=True, text=True, encoding="utf-8")
    try:
        rep = json.loads(r.stdout.strip().splitlines()[-1])
    except (json.JSONDecodeError, IndexError):
        rep = {}
    office = rep.get("office") or {}
    text = open(office["text_file"], encoding="utf-8").read() if office.get("text_file") else ""
    found = sum(1 for p in paragraphs if p in text)
    well_formed = rebuilt_ok = False
    try:
        ET.parse(os.path.join(out, "word", "document.xml"))
        well_formed = True
        with zipfile.ZipFile(office["rebuilt"]) as z:
            ET.fromstring(z.read("word/document.xml"))
            rebuilt_ok = True
    except Exception:  # noqa: BLE001
        pass
    ok = r.returncode == 2 and found >= 0.3 * len(paragraphs) and well_formed and rebuilt_ok
    failures += not ok
    print(f"{'ok  ' if ok else 'FAIL'} rescue of a docx with zeros in its document: {found} of {len(paragraphs)} paragraphs in the text, "
          f"document.xml well-formed {well_formed}, rebuilt document opens {rebuilt_ok}, exit {r.returncode}")

    # an ASCII-mode transfer (every LF made CR LF) is undone; but a sound ZIP of Windows text
    # stored as is, where nearly every LF has its CR too, must be left alone
    lines = "".join(f"line {i} of a Windows text file\r\n" for i in range(3000))
    for label, convert in (("a sound ZIP of stored CR LF text", False), ("a ZIP sent through FTP in ASCII mode", True)):
        z_path = os.path.join(work, "text.zip")
        with zipfile.ZipFile(z_path, "w", zipfile.ZIP_STORED) as z:
            for k in range(4):
                z.writestr(f"text{k}.txt", lines)
        raw = open(z_path, "rb").read()
        if convert:
            raw = raw.replace(b"\n", b"\r\n")  # the sender makes every LF a line end, CR or not
        bad = os.path.join(work, "text_case.zip")
        open(bad, "wb").write(raw)
        out = os.path.join(work, "rescued_text")
        shutil.rmtree(out, ignore_errors=True)
        r = subprocess.run([*GPUZIP, "rescue", bad, "-o", out, "--json"], capture_output=True, text=True, encoding="utf-8")
        try:
            rep = json.loads(r.stdout.strip().splitlines()[-1])
        except (json.JSONDecodeError, IndexError):
            rep = {}
        restored = all(os.path.exists(os.path.join(out, f"text{k}.txt"))
                       and open(os.path.join(out, f"text{k}.txt"), "rb").read() == lines.encode() for k in range(4))
        intact = rep.get("counts", {}).get("intact", 0)
        ok = restored and intact == 4 and rep.get("ascii_transfer_undone") == convert
        failures += not ok
        print(f"{'ok  ' if ok else 'FAIL'} rescue of {label}: {intact} of 4 entries intact, "
              f"ASCII transfer undone {rep.get('ascii_transfer_undone')}, exit {r.returncode}")

    # the reverse transfer (every CR LF made LF) removed bytes from compressed entries: the CRs
    # are put back where each entry's CRC says, and no entry called intact may differ
    rnd = random.Random(7)
    texts = {f"t{k}.txt": "".join(rnd.choice("abcdefghij klmnop,.\r\n") for _ in range(16000)).encode() for k in range(120)}
    z_path = os.path.join(work, "deflated.zip")
    with zipfile.ZipFile(z_path, "w", zipfile.ZIP_DEFLATED) as z:
        for n, t in texts.items():
            z.writestr(n, t)
    raw = open(z_path, "rb").read()
    lost = raw.count(b"\r\n")
    bad = os.path.join(work, "stripped.zip")
    open(bad, "wb").write(raw.replace(b"\r\n", b"\n"))
    out = os.path.join(work, "rescued_stripped")
    shutil.rmtree(out, ignore_errors=True)
    r = subprocess.run([*GPUZIP, "rescue", bad, "-o", out, "--json"], capture_output=True, text=True, encoding="utf-8")
    try:
        rep = json.loads(r.stdout.strip().splitlines()[-1])
    except (json.JSONDecodeError, IndexError):
        rep = {}
    status = {e["name"]: e["status"] for e in rep.get("entries", [])}
    back = {n for n, t in texts.items() if os.path.exists(os.path.join(out, n)) and open(os.path.join(out, n), "rb").read() == t}
    intact_wrong = [n for n in texts if status.get(n) == "intact" and n not in back]
    ok = not intact_wrong and len(back) >= len(texts) - 2 and rep.get("crs_restored", 0) >= 1
    failures += not ok
    print(f"{'ok  ' if ok else 'FAIL'} rescue of a ZIP whose CR LF pairs became LF ({lost} lost): {len(back)} of {len(texts)} "
          f"identical, {rep.get('crs_restored')} CRs put back, exit {r.returncode}"
          + (f"; CALLED INTACT BUT WRONG: {intact_wrong[:3]}" if intact_wrong else ""))

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
