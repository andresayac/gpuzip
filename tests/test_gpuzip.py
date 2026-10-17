"""Correctness tests for gpuzip: archives made by different tools must extract to exactly
the files they were made from, in every mode.

  python tests\\test_gpuzip.py                                                (the Python version)
  set GPUZIP_BIN=rust\\target\\release\\gpuzip.exe & python tests\\test_gpuzip.py   (the Rust build)
  GPUZIP_BIN=rust/target/release/gpuzip python3 tests/test_gpuzip.py            (Linux, macOS)

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
import json
import os
import random
import shutil
import subprocess
import sys
import tarfile
import tempfile
import zipfile
import zlib

HERE = os.path.dirname(os.path.abspath(__file__))
GPUZIP = [os.path.abspath(os.environ["GPUZIP_BIN"])] if os.environ.get("GPUZIP_BIN") else \
    [sys.executable, os.path.join(HERE, "..", "python", "gpuzip.py")]
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

    # rescue (Rust only): a .tar.gz with a zeroed stretch must give back nearly every file, the
    # damage must be reported, and no file the report calls intact may differ from the source
    if os.environ.get("GPUZIP_BIN"):
        src_tgz = archives["tar.gz (Python tarfile)"]
        files = {k: v for k, v in expected.items() if v[0] != "dir"}
        for where in (0.4, 0.85):  # the random blobs (stored blocks: only the CRC can tell), then text
            data = bytearray(open(src_tgz, "rb").read())
            p = int(len(data) * where)
            data[p:p + 2048] = bytes(2048)
            bad = os.path.join(work, f"damaged{int(where * 100)}.tar.gz")
            open(bad, "wb").write(bytes(data))
            out = os.path.join(work, f"rescued{int(where * 100)}")
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
            ok = r.returncode == 2 and reported and identical >= 0.5 * len(files) and identical >= intact
            failures += not ok
            print(f"{'ok  ' if ok else 'FAIL'} rescue of a tar.gz zeroed at {int(where * 100)}%: {identical} of {len(files)} files identical, "
                  f"{intact} called intact, {unverified} unverifiable, {len(t.get('partial', []))} partial, "
                  f"{rep.get('damaged_blocks', '?')} damaged blocks, exit {r.returncode}")

    # rescue of gzip files of one Deflate block (Rust only): a small text, and repetitive data that
    # packs megabytes into one block, with zeros written over the middle. Zeros decode as valid
    # codes: the decoder may fall back in step after them (only the CRC tells), or run on to the
    # end of the file. No byte called exact may differ from the original at its place, and the
    # bytes before the damage must be certified about as far as gzip decodes them right. Exact
    # bytes are told apart from unknown ones by rescuing with two fill bytes, from suspect ones by
    # the damaged list.
    if os.environ.get("GPUZIP_BIN"):
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

    # rescue of ZIPs (Rust only): without its central directory (cut, or zeroed), and with a zeroed
    # stretch inside; every entry called intact must be identical, the damage reported
    if os.environ.get("GPUZIP_BIN"):
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
        import random
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
