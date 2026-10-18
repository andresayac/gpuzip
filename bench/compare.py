"""gpuzip against the tools people already use, on the same archives: time to extract, and the
same bytes out.

  python bench/compare.py --gpuzip PATH [--ripunzip PATH] [--rapidgzip PATH] [--repeat 3]
                          [--work DIR] [--out results.json] ARCHIVE...

ZIP: gpuzip (auto, cpu), 7-Zip, ripunzip, and on Windows bsdtar (its tar.exe), PowerShell's
Expand-Archive and File Explorer; on Linux Info-ZIP's unzip. .gz: gpuzip, 7-Zip, rapidgzip,
GNU gzip, pigz (where found). tar.gz: gpuzip, bsdtar or GNU tar, 7-Zip (gz, then tar), tar with
pigz. Each extraction goes into an empty folder, the median of --repeat kept, process start
included; every tool's output must add up to the same size, or its time is not counted.
"""

import argparse
import json
import os
import shutil
import statistics
import subprocess
import sys
import tempfile
import time
import zipfile

HERE = os.path.dirname(os.path.abspath(__file__))
sys.path.insert(0, os.path.join(HERE, "..", "tests"))
from test_gpuzip import SEVENZIP  # noqa: E402

WIN = os.name == "nt"


def total_size(root):
    return sum(os.path.getsize(os.path.join(dp, f)) for dp, _, fs in os.walk(root) for f in fs
               if not os.path.islink(os.path.join(dp, f)))


def explorer(archive, out, expect):
    """File Explorer's own extraction (Shell.Application), waited for until every byte is there"""
    out, archive = os.path.normpath(out), os.path.normpath(archive)  # the Shell wants backslashes
    os.makedirs(out, exist_ok=True)
    ps = (f"$s = New-Object -ComObject Shell.Application; $d = $s.NameSpace('{out}'); "
          f"$d.CopyHere($s.NameSpace('{archive}').Items(), 0x614)")
    if subprocess.run(["powershell", "-NoProfile", "-Command", ps], capture_output=True).returncode:
        return
    deadline = time.time() + 1800
    while total_size(out) < expect and time.time() < deadline:
        time.sleep(0.2)


def tools(kind, a):
    """(name, function(archive, out)) for one kind of archive"""
    run = lambda *cmd, **kw: subprocess.run(list(cmd), check=False, **{"stdout": subprocess.DEVNULL, "stderr": subprocess.DEVNULL, **kw})
    g = a.gpuzip
    out = []
    if kind in ("zip", "gz", "tar.gz"):
        out += [("gpuzip", lambda f, o: run(g, "x", f, "-o", o, "-q")),
                ("gpuzip --mode cpu", lambda f, o: run(g, "x", f, "-o", o, "-q", "--mode", "cpu"))]
    if kind == "zip":
        out.append(("7-Zip", lambda f, o: run(SEVENZIP, "x", f, f"-o{o}", "-y")))
        if a.ripunzip:
            out.append(("ripunzip", lambda f, o: run(a.ripunzip, "unzip-file", f, "-d", o)))
        if WIN:
            out.append(("bsdtar (Windows tar.exe)", lambda f, o: (os.makedirs(o, exist_ok=True), run("C:\\Windows\\System32\\tar.exe", "-xf", f, "-C", o))))
            out.append(("PowerShell Expand-Archive", lambda f, o: run("powershell", "-NoProfile", "-Command",
                                                                       f"Expand-Archive -LiteralPath '{f}' -DestinationPath '{o}' -Force")))
            out.append(("File Explorer", lambda f, o: explorer(f, o, a.expect)))
        elif shutil.which("unzip"):
            out.append(("Info-ZIP unzip", lambda f, o: run("unzip", "-q", "-o", f, "-d", o)))
    if kind == "gz":
        name = lambda f: os.path.basename(f)[:-3]
        out.append(("7-Zip", lambda f, o: run(SEVENZIP, "x", f, f"-o{o}", "-y")))
        if a.rapidgzip:
            out.append(("rapidgzip", lambda f, o: (os.makedirs(o, exist_ok=True),
                                                   run(a.rapidgzip, "-d", "-P", str(os.cpu_count()), "-o", os.path.join(o, name(f)), f))))
        gz = shutil.which("gzip")
        if gz:
            out.append(("GNU gzip", lambda f, o: (os.makedirs(o, exist_ok=True),
                                                  run(gz, "-dc", f, stdout=open(os.path.join(o, name(f)), "wb")))))
        if shutil.which("pigz"):
            out.append(("pigz", lambda f, o: (os.makedirs(o, exist_ok=True),
                                              run("pigz", "-dc", f, stdout=open(os.path.join(o, name(f)), "wb")))))
    if kind == "tar.gz":
        tar = "C:\\Windows\\System32\\tar.exe" if WIN else "tar"
        out.append(("bsdtar (Windows tar.exe)" if WIN else "GNU tar", lambda f, o: (os.makedirs(o, exist_ok=True), run(tar, "-xzf", f, "-C", o))))
        if not WIN and shutil.which("pigz"):
            out.append(("tar with pigz", lambda f, o: (os.makedirs(o, exist_ok=True), run("tar", "-I", "pigz", "-xf", f, "-C", o))))

        def seven_two_step(f, o):
            os.makedirs(o, exist_ok=True)
            p = subprocess.Popen([SEVENZIP, "x", f, "-so"], stdout=subprocess.PIPE, stderr=subprocess.DEVNULL)
            subprocess.run([SEVENZIP, "x", "-si", "-ttar", f"-o{o}", "-y"], stdin=p.stdout, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
            p.wait()
        out.append(("7-Zip (gz, then tar)", seven_two_step))
    return out


def kind_of(path):
    p = path.lower()
    return "tar.gz" if p.endswith((".tar.gz", ".tgz")) else "gz" if p.endswith(".gz") else "zip"


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("archives", nargs="+")
    ap.add_argument("--gpuzip", required=True)
    ap.add_argument("--ripunzip")
    ap.add_argument("--rapidgzip")
    ap.add_argument("--repeat", type=int, default=3)
    ap.add_argument("--work", default=os.path.join(tempfile.gettempdir(), "gpuzip-bench", "compare"))
    ap.add_argument("--out", default=None)
    a = ap.parse_args()
    a.work = os.path.normpath(os.path.abspath(a.work))
    os.makedirs(a.work, exist_ok=True)
    rows = []
    for f in a.archives:
        f = os.path.abspath(f)
        k = kind_of(f)
        out = os.path.join(a.work, "out")
        shutil.rmtree(out, ignore_errors=True)
        subprocess.run([a.gpuzip, "x", f, "-o", out, "-q", "--mode", "cpu"], check=True)
        a.expect = total_size(out)
        shutil.rmtree(out, ignore_errors=True)
        print(f"{os.path.basename(f)}: {os.path.getsize(f) / 1e6:,.0f} MB, {a.expect / 1e9:.2f} GB out", flush=True)
        for name, fn in tools(k, a):
            times, size = [], None
            for _ in range(a.repeat):
                shutil.rmtree(out, ignore_errors=True)
                t = time.perf_counter()
                try:
                    fn(f, out)
                except Exception as e:  # noqa: BLE001  a tool that cannot run is not counted
                    print(f"    {name}: {e}", flush=True)
                times.append(time.perf_counter() - t)
                size = total_size(out) if os.path.isdir(out) else 0
                if size != a.expect:
                    break
            shutil.rmtree(out, ignore_errors=True)
            ok = size == a.expect
            med = statistics.median(times) if ok else None
            rows.append({"archive": os.path.basename(f), "tool": name, "seconds": round(med, 2) if ok else None,
                         "runs": [round(x, 2) for x in times], "same_size": ok, "size": size})
            print(f"  {name:<28} " + (f"{med:7.2f} s  (runs {', '.join(f'{x:.2f}' for x in times)})" if ok
                                      else f"wrote {size:,} bytes of {a.expect:,}: not counted"), flush=True)
    path = a.out or os.path.join(a.work, "compare.json")
    json.dump(rows, open(path, "w"), indent=1)
    print(f"\n{path}")


if __name__ == "__main__":
    main()
