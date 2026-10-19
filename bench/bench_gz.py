"""Benchmark gpuzip on gzip files against 7-Zip, GNU gzip, rapidgzip and Windows' tar.

  python bench\\bench_gz.py --data %TEMP%\\gpuzip-bench [--runs 3] [--rapidgzip PATH] [--gzip PATH]

Datasets (made in --data when missing):
  text.gz     128 MB of text, 7-Zip -mx5          (from research/gpu-inflate's inputs, if there)
  big6.txt.gz 6.4 GB of text, zlib level 4        (gpuinflate/make_big_gz.py)
  medium.tar.gz  the 2,000 files of medium.zip, as a tar.gz from Python's tarfile (level 6)
Jobs:
  test     decompress and check the CRC, write nothing
  extract  write the file (or, for the tar.gz, every file in it)
Every command runs once to warm the caches, then --runs rounds follow, the tools taking turns
inside each round; the median wall time, process start included, is reported. Extractions are
compared with the expected bytes (one file) or with 7-Zip's extraction (the tar.gz).
Results go to results-gz.json and results-gz.md next to this script.
"""

import argparse
import hashlib
import json
import os
import platform
import shutil
import statistics
import subprocess
import sys
import tarfile
import time

HERE = os.path.dirname(os.path.abspath(__file__))
ROOT = os.path.dirname(HERE)
RUST = os.path.join(ROOT, "target", "release", "gpuzip.exe")
SEVENZIP = r"C:\Program Files\7-Zip\7z.exe"
TAR = r"C:\Windows\System32\tar.exe"


def sha_file(path):
    h = hashlib.sha256()
    with open(path, "rb") as f:
        while b := f.read(16 << 20):
            h.update(b)
    return h.hexdigest()


def sha_tree(root):
    out = {}
    for d, _, files in os.walk(root):
        for n in files:
            p = os.path.join(d, n)
            out[os.path.relpath(p, root).replace("\\", "/")] = sha_file(p)
    return out


def run(cmd, stdout=None, shell=False):
    t = time.perf_counter()
    r = subprocess.run(cmd, stdout=stdout or subprocess.DEVNULL, stderr=subprocess.DEVNULL, shell=shell)
    return time.perf_counter() - t, r.returncode


def datasets(data):
    out = {}
    src = os.path.join(data, "inflate", "7zip-mx5.gz")
    if os.path.exists(src):
        out["text.gz"] = ("128 MB of text, 7-Zip -mx5", src, os.path.join(data, "inflate", "text.bin"))
    big = os.path.join(data, "big6.txt.gz")
    if not os.path.exists(big) and os.path.exists(os.path.join(data, "inflate", "text.bin")):
        subprocess.run([sys.executable, os.path.join(ROOT, "gpuinflate", "make_big_gz.py"),
                        os.path.join(data, "inflate", "text.bin"), big, "6"], check=True)
    if os.path.exists(big):
        out["big6.txt.gz"] = ("6.4 GB of text, zlib level 4", big, None)
    tgz = os.path.join(data, "medium.tar.gz")
    if not os.path.exists(tgz) and os.path.exists(os.path.join(data, "medium.zip")):
        print("  building medium.tar.gz...", flush=True)
        tmp = os.path.join(data, "medium-src")
        shutil.rmtree(tmp, ignore_errors=True)
        subprocess.run([SEVENZIP, "x", os.path.join(data, "medium.zip"), f"-o{tmp}", "-y"], check=True,
                       stdout=subprocess.DEVNULL)
        with tarfile.open(tgz + ".partial", "w:gz", compresslevel=6) as t:
            for e in sorted(os.listdir(tmp)):
                t.add(os.path.join(tmp, e), e)
        os.replace(tgz + ".partial", tgz)
        shutil.rmtree(tmp, ignore_errors=True)
    if os.path.exists(tgz):
        out["medium.tar.gz"] = ("2,000 files (1.4 GB) in a tar.gz, Python tarfile level 6", tgz, None)
    return out


def tools(args, name, path, out):
    """job -> list of (label, function returning (seconds, returncode))"""
    tar = name.endswith(".tar.gz")
    rg, gz = args.rapidgzip, args.gzip
    t = {"test": [], "extract": []}
    t["test"].append(("7-Zip", lambda: run([SEVENZIP, "t", path])))
    if gz:
        t["test"].append(("GNU gzip", lambda: run([gz, "-t", path])))
    if rg:
        t["test"].append(("rapidgzip, 16 threads", lambda: run([rg, "--test", path])))
    for m in ("cpu", "gpu", "auto"):
        t["test"].append((f"gpuzip {m}", lambda m=m: run([RUST, "t", path, "--mode", m])))
    if name == "big6.txt.gz" and not args.big_extract:
        return t
    if tar:
        t["extract"].append(("7-Zip (gz, then tar)", lambda: run(
            f'"{SEVENZIP}" x -so "{path}" | "{SEVENZIP}" x -si -ttar -y "-o{out}"', shell=True)))
        t["extract"].append(("Windows tar", lambda: (os.makedirs(out, exist_ok=True), run([TAR, "-xzf", path, "-C", out]))[1]))
        if rg:
            t["extract"].append(("rapidgzip, then Windows tar", lambda: (os.makedirs(out, exist_ok=True), run(
                f'"{rg}" -d -c "{path}" | "{TAR}" -xf - -C "{out}"', shell=True))[1]))
    else:
        t["extract"].append(("7-Zip", lambda: run([SEVENZIP, "x", path, f"-o{out}", "-y"])))
        target = os.path.join(out, os.path.basename(path)[:-3])
        if gz:
            def gnu():
                os.makedirs(out, exist_ok=True)
                with open(target, "wb") as f:
                    return run([gz, "-dc", path], stdout=f)
            t["extract"].append(("GNU gzip", gnu))
        if rg:
            t["extract"].append(("rapidgzip, 16 threads", lambda: (os.makedirs(out, exist_ok=True), run(
                [rg, "-d", "-f", "-o", target, path]))[1]))
    for m in ("cpu", "auto"):
        t["extract"].append((f"gpuzip {m}", lambda m=m: run([RUST, "x", path, "-o", out, "--mode", m])))
    return t


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--data", required=True)
    ap.add_argument("--runs", type=int, default=3)
    ap.add_argument("--rapidgzip", default=os.path.join(ROOT, ".venv", "Scripts", "rapidgzip.exe"))
    ap.add_argument("--gzip", default=r"C:\Program Files\Git\usr\bin\gzip.exe")
    ap.add_argument("--big-extract", action="store_true", help="also extract the 6.4 GB file (writes 6.4 GB per run)")
    args = ap.parse_args()
    args.rapidgzip = args.rapidgzip if os.path.exists(args.rapidgzip) else None
    args.gzip = args.gzip if os.path.exists(args.gzip) else None
    results = {"machine": platform.processor(), "runs": args.runs, "datasets": {}}
    out = os.path.join(args.data, "bench-gz-out")
    for name, (desc, path, expect) in datasets(args.data).items():
        print(f"{name}: {desc}", flush=True)
        res = results["datasets"][name] = {"description": desc, "packed": os.path.getsize(path), "jobs": {}}
        reference = None
        for job, ts in tools(args, name, path, out).items():
            if not ts:
                continue
            times = {label: [] for label, _ in ts}
            for rnd in range(args.runs + 1):  # round 0 warms the caches
                for label, f in ts:
                    shutil.rmtree(out, ignore_errors=True)
                    secs, rc = f()
                    if rc != 0:
                        times[label] = None
                        print(f"  {job} {label}: exit code {rc}")
                    elif times[label] is not None and rnd > 0:
                        times[label].append(secs)
                    if job == "extract" and rnd == args.runs and times[label] is not None:
                        got = sha_tree(out)
                        if expect:
                            want = {os.path.basename(path)[:-3]: sha_file(expect)} if reference is None else reference
                            reference = want
                        else:
                            reference = reference or got
                            want = reference
                        if got != want:
                            print(f"  {job} {label}: OUTPUT DIFFERS")
                            times[label] = None
            res["jobs"][job] = {k: (statistics.median(v) if v else None) for k, v in times.items()}
            for k, v in res["jobs"][job].items():
                print(f"  {job:8} {k:26} {'failed' if v is None else f'{v:7.2f} s'}", flush=True)
    shutil.rmtree(out, ignore_errors=True)
    with open(os.path.join(HERE, "results-gz.json"), "w") as f:
        json.dump(results, f, indent=1)
    lines = ["| Dataset | Job | Tool | Time | vs 7-Zip |", "|---|---|---|---|---|"]
    for name, r in results["datasets"].items():
        for job, row in r["jobs"].items():
            base = next(iter(row.values()))
            for tool, secs in row.items():
                rel = f"{base / secs:.1f}x" if secs and base else "-"
                lines.append(f"| {name} | {job} | {tool} | {'failed' if secs is None else f'{secs:.2f} s'} | {rel} |")
    with open(os.path.join(HERE, "results-gz.md"), "w") as f:
        f.write("\n".join(lines) + "\n")
    print("\n".join(lines))


main()
