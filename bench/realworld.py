"""gpuzip on real archives: every mode, every extracted byte checked against 7-Zip.

  python bench/realworld.py [--bin LABEL=PATH ...] [--modes cpu,gpu,auto] [--work DIR]
                            [--max-extract-gb 12] [--rescue] ARCHIVE_OR_FOLDER...

For each archive 7-Zip extracts it once as the reference (kept in WORK/ref); then every gpuzip
given with --bin, in every mode, extracts it once (`x --json`) and tests it three times (`t
--json`, the median kept), and every
file is compared with the reference by SHA-256. Archives that unpack to more than
--max-extract-gb are only tested. names.zip and slip.zip (from make_realworld.py) are checked
for what they are about instead: no crash, nothing written outside the output folder.

--rescue instead damages each ZIP and tar.gz of 1 GB or less in the eleven ways of
research/recovery/realistic.py and runs `gpuzip rescue` on each: how many files come back
identical, how many it calls intact although they differ (silent errors, which must be 0), and the
most memory each run held.

Results go to WORK/report/realworld*.json and .md. The public downloads this was run on:
linux-6.6.tar.gz (kernel.org), Python-3.14.0.tgz (python.org), enwik9.zip (mattmahoney.net),
silesia.zip (sun.aei.polsl.pl), cpython v3.13.0 as a GitHub zip; plus make_realworld.py's.
"""

import argparse
import hashlib
import json
import os
import random
import shutil
import subprocess
import sys
import time
import zipfile

HERE = os.path.dirname(os.path.abspath(__file__))
sys.path.insert(0, os.path.join(HERE, "..", "tests"))
sys.path.insert(0, os.path.join(HERE, "..", "research", "recovery"))
from test_gpuzip import SEVENZIP  # noqa: E402

PASSWORD = "correct horse"
SOURCE = None  # --source: the folder make_realworld.py's archives were made from
ZIPLIKE = (".zip", ".jar", ".docx", ".xlsx", ".pptx", ".apk", ".whl", ".epub")
TARGZ = (".tar.gz", ".tgz")
OTHER = (".rar", ".7z")
SPECIAL = ("names.zip", "slip.zip")


def kind(path):
    p = path.lower()
    if p.endswith(TARGZ):
        return "tar.gz"
    if p.endswith(".gz"):
        return "gz"
    if p.endswith(ZIPLIKE):
        return "zip"
    if p.endswith(OTHER):
        return "other"
    return None


def find_archives(args):
    out = []
    for a in args:
        if os.path.isdir(a):
            for dp, dirs, files in os.walk(a):
                dirs[:] = [d for d in dirs if not d.startswith("_")]
                out += [os.path.join(dp, f) for f in sorted(files) if kind(f)]
        elif kind(a):
            out.append(a)
    return out


def tree(root):
    """relpath -> sha256 of every regular file below root"""
    out = {}
    for dp, _, files in os.walk(root):
        for f in files:
            p = os.path.join(dp, f)
            if os.path.islink(p):
                continue
            h = hashlib.sha256()
            with open(p, "rb") as fh:
                while chunk := fh.read(1 << 22):
                    h.update(chunk)
            out[os.path.relpath(p, root).replace("\\", "/")] = h.hexdigest()
    return out


def compare(ref, got):
    missing = sorted(set(ref) - set(got))
    extra = sorted(set(got) - set(ref))
    differs = sorted(k for k in set(ref) & set(got) if ref[k] != got[k])
    names_only = bool(missing or extra) and not differs and sorted(ref.values()) == sorted(got.values())
    return {"missing": len(missing), "extra": len(extra), "differs": len(differs), "names_only": names_only,
            "examples": (missing + extra + differs)[:5]}


def peak_memory(p):
    """The finished process's peak memory, in bytes, read from the system rather than sampled
    (Windows: its peak working set; elsewhere: wait4's maxrss). Reaps the process."""
    if os.name == "nt":
        import ctypes
        from ctypes import wintypes

        class Counters(ctypes.Structure):
            _fields_ = [("cb", wintypes.DWORD), ("PageFaultCount", wintypes.DWORD)] + [
                (f, ctypes.c_size_t) for f in ("PeakWorkingSetSize", "WorkingSetSize", "QuotaPeakPagedPoolUsage",
                                              "QuotaPagedPoolUsage", "QuotaPeakNonPagedPoolUsage",
                                              "QuotaNonPagedPoolUsage", "PagefileUsage", "PeakPagefileUsage")]
        p.wait()
        c = Counters(cb=ctypes.sizeof(Counters))
        ok = ctypes.windll.kernel32.K32GetProcessMemoryInfo(wintypes.HANDLE(int(p._handle)), ctypes.byref(c), c.cb)
        return c.PeakWorkingSetSize if ok else 0
    _, status, usage = os.wait4(p.pid, 0)
    p.returncode = os.waitstatus_to_exitcode(status)
    return usage.ru_maxrss * (1 if sys.platform == "darwin" else 1024)


def run(cmd, cwd=None):
    t = time.perf_counter()
    p = subprocess.Popen(cmd, stdout=subprocess.PIPE, stderr=subprocess.STDOUT, text=True, encoding="utf-8",
                         errors="replace", cwd=cwd)
    out = p.stdout.read()
    p.stdout.close()
    peak = peak_memory(p)
    secs = time.perf_counter() - t
    rep = {}
    for line in reversed(out.strip().splitlines()):
        try:
            rep = json.loads(line)
            break
        except json.JSONDecodeError:
            continue
    return p.returncode, secs, rep, out[-600:], peak


def unpacked_size(path):
    k = kind(path)
    if k == "zip":
        try:
            with zipfile.ZipFile(path) as z:
                return sum(i.file_size for i in z.infolist())
        except Exception:  # noqa: BLE001
            return os.path.getsize(path)
    if k == "other":
        return 4 * os.path.getsize(path)
    return 4 * os.path.getsize(path)  # gzip's size field wraps at 4 GB: a guess


def reference(path, work, cache):
    """7-Zip's extraction of path, as a tree of hashes; cached by path, size and time"""
    key = f"{os.path.abspath(path)}|{os.path.getsize(path)}|{os.path.getmtime(path)}"
    if key in cache:
        return cache[key]
    dest = os.path.join(work, "ref", "current")
    shutil.rmtree(dest, ignore_errors=True)
    os.makedirs(dest)
    t = time.perf_counter()
    pw = [f"-p{PASSWORD}"]
    if kind(path) == "tar.gz":
        first = subprocess.Popen([SEVENZIP, "x", path, "-so", "-y"], stdout=subprocess.PIPE, stderr=subprocess.DEVNULL)
        rc2 = subprocess.run([SEVENZIP, "x", "-si", "-ttar", "-y", f"-o{dest}"], stdin=first.stdout,
                             stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL).returncode
        rc = first.wait() or rc2
    else:
        rc = subprocess.run([SEVENZIP, "x", path, "-y", f"-o{dest}", *pw], stdout=subprocess.DEVNULL,
                            stderr=subprocess.DEVNULL).returncode
    secs = time.perf_counter() - t
    result = {"rc": rc, "seconds": round(secs, 2), "tree": tree(dest), "by": "7-Zip"}
    shutil.rmtree(dest, ignore_errors=True)
    if rc != 0 and kind(path) == "zip":
        # 7-Zip refused it (a self-extractor's stub, local names that differ from the directory's):
        # Python's zipfile, or the folder make_realworld.py made the archive from
        try:
            with zipfile.ZipFile(path) as z:
                result["tree"] = {i.filename.replace("\\", "/"): hashlib.sha256(z.read(i)).hexdigest()
                                  for i in z.infolist() if not i.is_dir()}
                result["by"] = "Python zipfile (7-Zip failed)"
        except Exception:  # noqa: BLE001
            if SOURCE and os.path.isdir(SOURCE):
                result["tree"], result["by"] = tree(SOURCE), "the source folder (7-Zip and zipfile failed)"
    cache[key] = result
    return result


def special(path, bins, modes, work):
    """names.zip and slip.zip: no crash, nothing outside the output folder"""
    rows = []
    escapes = [os.path.join(work, "sandbox", "evil1.txt"), "C:\\abs\\evil2.txt", "\\abs\\evil2.txt", "C:\\evil3.txt",
               os.path.join(work, "sandbox", "evil4.txt"), os.path.join(work, "sandbox", "evil5.txt")]
    for label, exe in bins:
        for mode in modes:
            sandbox = os.path.join(work, "sandbox")
            shutil.rmtree(sandbox, ignore_errors=True)
            out = os.path.join(sandbox, "out")
            os.makedirs(sandbox)
            rc, secs, rep, tail, _ = run([exe, "x", path, "-o", out, "--mode", mode, "--json"])
            outside = [p for p in os.listdir(sandbox) if p != "out"] + [p for p in escapes if os.path.exists(p)]
            got = sorted(tree(out)) if os.path.isdir(out) else []
            ok = rc in (0, 2) and not outside and "panicked" not in tail
            rows.append({"archive": os.path.basename(path), "bin": label, "mode": mode, "rc": rc,
                         "seconds": round(secs, 2), "ok": ok, "outside": outside, "extracted": got,
                         "failed": [f.get("name") for f in rep.get("failed", [])], "tail": "" if ok else tail})
    shutil.rmtree(os.path.join(work, "sandbox"), ignore_errors=True)
    return rows


def extraction(archives, bins, modes, work, max_gb, cache_path):
    cache = json.load(open(cache_path)) if os.path.exists(cache_path) else {}
    rows = []
    for path in archives:
        name = os.path.basename(path)
        if name in SPECIAL:
            rows += special(path, bins, modes, work)
            continue
        size = unpacked_size(path)
        test_only = size > max_gb * 1e9
        ref = None
        if not test_only:
            ref = reference(path, work, cache)
            json.dump(cache, open(cache_path, "w"))
        print(f"{name}: {os.path.getsize(path) / 1e6:,.0f} MB, ~{size / 1e9:.1f} GB unpacked"
              + (" (test only)" if test_only else f", 7-Zip {ref['seconds']} s, {len(ref['tree'])} files"
                 + ("" if ref.get("by", "7-Zip") == "7-Zip" else f", reference: {ref['by']}")), flush=True)
        extra = ["--password", PASSWORD] if any(s in name for s in ("aes", "zipcrypto")) else []
        handed_off = kind(path) == "other"  # RAR, 7z: 7-Zip does the work whatever the mode
        for label, exe in bins:
            for mode in (["auto"] if handed_off else modes):
                row = {"archive": name, "bin": label, "mode": mode, "size": os.path.getsize(path)}
                times = []
                for _ in range(1 if handed_off else 3):  # testing is cheap: the median of three
                    rc, secs, rep, tail, _ = run([exe, "t", path, "--mode", mode, "--json", *extra])
                    times.append(secs)
                    if rc != 0:
                        break
                secs = sorted(times)[len(times) // 2]
                row.update(t_rc=rc, t_seconds=round(secs, 2), t_failed=len(rep.get("failed", [])),
                           routes=rep.get("routes"), notes=rep.get("notes", []))
                if not test_only:
                    out = os.path.join(work, "out")
                    shutil.rmtree(out, ignore_errors=True)
                    rc, secs, rep, tail, _ = run([exe, "x", path, "-o", out, "--mode", mode, "--json", *extra])
                    got = tree(out) if os.path.isdir(out) else {}
                    if kind(path) == "gz":  # one file, named by gpuzip and 7-Zip each in its own way
                        cmp = compare({"file": v for v in ref["tree"].values()}, {"file": v for v in got.values()})
                    else:
                        cmp = compare(ref["tree"], got)
                    shutil.rmtree(out, ignore_errors=True)
                    row.update(x_rc=rc, x_seconds=round(secs, 2), x_failed=len(rep.get("failed", [])), compare=cmp,
                               x_notes=rep.get("notes", []), x_routes=rep.get("routes"))
                    ok = rc == 0 and not (cmp["missing"] or cmp["extra"] or cmp["differs"])
                    ok = ok or (cmp["names_only"] and rc == 0)
                else:
                    ok = rc == 0
                row["ok"] = ok
                row["tail"] = "" if ok else tail
                rows.append(row)
                gpu = (row.get("x_routes") or row.get("routes") or {}).get("gpu", {}).get("files", 0)
                print(f"  {'ok  ' if ok else 'FAIL'} {label:<10} {mode:<5} t {row['t_seconds']:7.2f} s"
                      + (f"  x {row['x_seconds']:7.2f} s" if not test_only else "")
                      + f"  gpu files {gpu}" + (f"  {row.get('compare')}" if not ok else ""), flush=True)
    return rows


def rescue_matrix(archives, bins, work, cache_path, only=None):
    import realistic  # research/recovery: the eleven ways files break
    cache = json.load(open(cache_path)) if os.path.exists(cache_path) else {}
    rows = []
    other = random.Random(1).randbytes(1 << 20)  # what a cross-linked cluster holds instead
    for path in archives:
        k = kind(path)
        if k not in ("zip", "tar.gz") or os.path.getsize(path) > 1e9 or os.path.basename(path) in SPECIAL:
            continue
        name = os.path.basename(path)
        if any(s in name for s in ("aes", "zipcrypto")):
            continue  # rescue does not take passwords
        ref = reference(path, work, cache)["tree"]
        json.dump(cache, open(cache_path, "w"))
        data = open(path, "rb").read()
        print(f"{name}: {len(ref)} files", flush=True)
        for failure, damage in realistic.FAILURES.items():
            if only and failure not in only:
                continue
            rng = random.Random(f"{name}/{failure}")
            bad, _ = damage(data, rng, other) if failure == "crosslinked" else damage(data, rng)
            ext = ".tar.gz" if k == "tar.gz" else os.path.splitext(name)[1]
            dpath = os.path.join(work, "damaged", f"{name}.{failure}{ext}")
            os.makedirs(os.path.dirname(dpath), exist_ok=True)
            open(dpath, "wb").write(bad)
            for label, exe in bins:
                out = os.path.join(work, "rescued")
                shutil.rmtree(out, ignore_errors=True)
                rc, secs, rep, tail, peak = run([exe, "rescue", dpath, "-o", out, "--json"])
                got = tree(out) if os.path.isdir(out) else {}
                identical = sum(1 for f, h in ref.items() if got.get(f) == h)
                if k == "zip":
                    status = {e["name"]: e["status"] for e in rep.get("entries", [])}
                    silent = sum(1 for f, h in ref.items() if status.get(f) == "intact" and got.get(f) != h)
                    # an entry found by scanning may come out under another name than the reference's:
                    # still a silent error if what it holds is no file of the archive
                    contents = set(ref.values())
                    silent += sum(1 for f, s in status.items() if s == "intact" and f not in ref and got.get(f) not in contents)
                    called = sum(1 for s in status.values() if s == "intact")
                else:
                    t = rep.get("tar") or {}
                    flagged = set(t.get("inferred", [])) | set(t.get("suspect", [])) | set(t.get("partial", []))
                    silent = 0 if t.get("unverified") else sum(
                        1 for f, h in ref.items() if f in got and got[f] != h and f not in flagged)
                    called = t.get("intact", 0)
                shutil.rmtree(out, ignore_errors=True)
                # an archive of no files has nothing to find once damaged: exit 1 says so
                ok = silent == 0 and (rc in (0, 2) or not ref) and "panicked" not in tail
                rows.append({"archive": name, "failure": failure, "bin": label, "rc": rc, "seconds": round(secs, 2),
                             "files": len(ref), "identical": identical, "called_intact": called, "silent": silent,
                             "peak_mb": peak >> 20, "ok": ok, "tail": "" if ok else tail})
                print(f"  {'ok  ' if ok else 'FAIL'} {failure:<14} {label:<10} {identical:>6} of {len(ref)} identical, "
                      f"{called} called intact, silent {silent}, {secs:6.2f} s, {peak / 2**30:5.2f} GB", flush=True)
            os.remove(dpath)
    return rows


def markdown(rows, rescue):
    lines = []
    if rescue:
        lines += ["| Archive | Failure | gpuzip | Identical | Called intact | Silent errors | Seconds | Peak memory |",
                  "|---|---|---|---|---|---|---|---|"]
        for r in rows:
            lines.append(f"| {r['archive']} | {r['failure']} | {r['bin']} | {r['identical']} of {r['files']} | "
                         f"{r['called_intact']} | {'**' + str(r['silent']) + '**' if r['silent'] else 0} | {r['seconds']} | "
                         f"{r.get('peak_mb', 0) / 1024:.2f} GB |")
        return "\n".join(lines) + "\n"
    lines += ["| Archive | gpuzip | Mode | Test (s) | Extract (s) | GPU files | Result |", "|---|---|---|---|---|---|---|"]
    for r in rows:
        if "extracted" in r:  # names.zip, slip.zip
            lines.append(f"| {r['archive']} | {r['bin']} | {r['mode']} | | {r['seconds']} | | "
                         f"{'ok' if r['ok'] else 'FAIL'}: {len(r['extracted'])} written, outside {r['outside'] or 'none'} |")
            continue
        gpu = (r.get("x_routes") or r.get("routes") or {}).get("gpu", {}).get("files", 0)
        res = "ok" if r["ok"] else f"FAIL {r.get('compare') or r.get('tail', '')[-120:]}"
        if r.get("compare", {}).get("names_only"):
            res += " (names differ from 7-Zip's, contents the same)"
        lines.append(f"| {r['archive']} | {r['bin']} | {r['mode']} | {r['t_seconds']} | {r.get('x_seconds', '')} | {gpu} | {res} |")
    return "\n".join(lines) + "\n"


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("paths", nargs="+")
    ap.add_argument("--bin", action="append", default=[], help="LABEL=PATH to a gpuzip (several to compare)")
    ap.add_argument("--modes", default="cpu,gpu,auto")
    ap.add_argument("--work", default=os.path.join(os.environ.get("TEMP", "/tmp"), "gpuzip-bench", "realworld", "work"))
    ap.add_argument("--max-extract-gb", type=float, default=12)
    ap.add_argument("--rescue", action="store_true")
    ap.add_argument("--failures", help="--rescue: only these failures, comma-separated (archives of many files are slow)")
    ap.add_argument("--source", help="make_realworld.py's _source folder: the reference when 7-Zip and zipfile fail")
    a = ap.parse_args()
    global SOURCE
    SOURCE = a.source
    bins = [tuple(b.split("=", 1)) for b in a.bin] or [("gpuzip", shutil.which("gpuzip") or "gpuzip")]
    os.makedirs(a.work, exist_ok=True)
    archives = find_archives(a.paths)
    cache_path = os.path.join(a.work, "ref.json")
    if a.rescue:
        rows = rescue_matrix(archives, bins, a.work, cache_path, set(a.failures.split(",")) if a.failures else None)
    else:
        rows = extraction(archives, bins, a.modes.split(","), a.work, a.max_extract_gb, cache_path)
    report = os.path.join(a.work, "report")
    os.makedirs(report, exist_ok=True)
    stem = "realworld-rescue" if a.rescue else "realworld"
    json.dump(rows, open(os.path.join(report, stem + ".json"), "w"), indent=1)
    open(os.path.join(report, stem + ".md"), "w", encoding="utf-8").write(markdown(rows, a.rescue))
    bad = [r for r in rows if not r["ok"]]
    print(f"\n{len(rows) - len(bad)} of {len(rows)} ok; report in {report}")
    return 1 if bad else 0


if __name__ == "__main__":
    sys.exit(main())
