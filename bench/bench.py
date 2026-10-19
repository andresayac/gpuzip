"""Benchmark gpuzip against 7-Zip on ZIP archives made by 7-Zip itself.

  python bench\\bench.py --data D:\\bench [--runs 3] [--only small,mixed] [--rust PATH]

Builds four datasets (many small files, medium files, a few large files, a realistic mix),
packs each with 7-Zip (zip, Deflate, -mx5), then times, per archive:
  test     7z t        vs gpuzip t --mode cpu|gpu|auto   decompress + CRC, nothing written
  extract  7z x        vs gpuzip x --mode cpu|auto       full extraction to disk
for the build in target/release (or --rust PATH). Every command runs once to warm the
caches, then --runs rounds follow, the tools taking turns inside each round; the median wall
time, including process start, is reported. The last extraction (gpuzip auto) is compared
file by file with 7-Zip's. Results go to results.json and results.md next to this script.
"""

import argparse
import hashlib
import json
import math
import os
import platform
import random
import re
import shutil
import statistics
import subprocess
import sys
import time

HERE = os.path.dirname(os.path.abspath(__file__))
ROOT = os.path.dirname(HERE)
RUST = os.path.join(ROOT, "target", "release", "gpuzip.exe")
SEVENZIP = r"C:\Program Files\7-Zip\7z.exe"

DATASETS = {
    # name: (description, list of (count, min_size, max_size, compressible))
    "small": ("40,000 small files (1-256 KB)", [(40_000, 1 << 10, 256 << 10, True)]),
    "medium": ("2,000 medium files (128 KB-2 MB)", [(2_000, 128 << 10, 2 << 20, True)]),
    "large": ("8 large files (128 MB)", [(8, 128 << 20, 128 << 20, True)]),
    "mixed": ("10,000 small + 300 medium + 4 large, 10% incompressible",
              [(9_000, 1 << 10, 256 << 10, True), (1_000, 1 << 10, 256 << 10, False),
               (300, 128 << 10, 2 << 20, True), (4, 64 << 20, 64 << 20, True)]),
}


def make_corpus(rnd, size=64 << 20):
    words = ["".join(rnd.choice("etaoinshrdlucmfwypvbgkjqxz") for _ in range(rnd.randint(2, 11)))
             for _ in range(20_000)]
    weights = [1 / (i + 1) for i in range(len(words))]
    seps = [" "] * 12 + [", ", ". ", "\n", ": ", "; ", "\t", " = ", " (", ") "]
    out, total = [], 0
    while total < size:
        chunk = rnd.choices(words, weights, k=200_000)
        line = "".join(w + rnd.choice(seps) + (str(rnd.randint(0, 99999)) + " " if rnd.random() < .05 else "")
                       for w in chunk)
        out.append(line)
        total += len(line)
    return "".join(out).encode()[:size]


def build_dataset(name, root):
    src = os.path.join(root, name)
    if os.path.isdir(src):
        return src
    print(f"  building {name} files...", flush=True)
    rnd = random.Random(name)
    corpus = make_corpus(rnd)
    tmp = src + ".partial"
    shutil.rmtree(tmp, ignore_errors=True)
    n = 0
    for count, lo, hi, compressible in DATASETS[name][1]:
        for _ in range(count):
            size = int(min(hi, max(lo, rnd.lognormvariate(math.log(math.sqrt(lo * hi)), 1.0))))
            d = os.path.join(tmp, f"d{n % 97:02}", f"s{n % 13}")
            os.makedirs(d, exist_ok=True)
            with open(os.path.join(d, f"f{n:06}.{'txt' if compressible else 'bin'}"), "wb") as f:
                if compressible:
                    while size:
                        off = rnd.randrange(0, len(corpus) - min(size, len(corpus) // 2))
                        part = corpus[off:off + min(size, len(corpus) // 2)]
                        f.write(part)
                        size -= len(part)
                else:
                    f.write(rnd.randbytes(size))
            n += 1
    os.rename(tmp, src)
    return src


def build_archive(name, root):
    zp = os.path.join(root, f"{name}.zip")
    if not os.path.exists(zp):
        src = build_dataset(name, root)
        print(f"  packing {name}.zip with 7-Zip...", flush=True)
        subprocess.run([SEVENZIP, "a", "-tzip", "-mx=5", zp + ".partial", "."], cwd=src, check=True,
                       stdout=subprocess.DEVNULL)
        os.rename(zp + ".partial", zp)
    return zp


def timed(cmd, out_dir=None):
    if out_dir:
        shutil.rmtree(out_dir, ignore_errors=True)
    t = time.perf_counter()
    r = subprocess.run(cmd, capture_output=True, text=True)
    secs = time.perf_counter() - t
    if r.returncode != 0:
        raise RuntimeError(f"{cmd} exited {r.returncode}: {r.stdout[-400:]} {r.stderr[-400:]}")
    report = None
    if "--json" in cmd:
        report = json.loads(r.stdout.strip().splitlines()[-1])
    return secs, report


def tree_digest(root):
    out = {}
    for dirpath, _, files in os.walk(root):
        for name in files:
            p = os.path.join(dirpath, name)
            with open(p, "rb") as f:
                out[os.path.relpath(p, root)] = hashlib.blake2b(f.read(), digest_size=16).digest()
    return out


def system_info(rust=RUST):
    gpu = subprocess.run(["nvidia-smi", "--query-gpu=name,driver_version", "--format=csv,noheader"],
                         capture_output=True, text=True).stdout.strip()
    cpu = subprocess.run(["powershell", "-NoProfile", "-Command", "(Get-CimInstance Win32_Processor).Name"],
                         capture_output=True, text=True).stdout.strip()
    sz = subprocess.run([SEVENZIP], capture_output=True, text=True).stdout.strip().splitlines()[0]
    # the nvCOMP gpuzip loads, as its doctor reports it
    doctor = subprocess.run([rust, "doctor"], capture_output=True, text=True).stdout if os.path.isfile(rust) else ""
    found = re.search(r"nvCOMP (\d[\d.]*)", doctor)
    nv = found.group(1) if found else "?"
    return {"gpu": gpu, "cpu": cpu, "threads": os.cpu_count(), "7zip": sz, "nvcomp": nv,
            "python": platform.python_version(), "os": platform.platform()}


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--data", required=True, help="folder for the generated files and archives")
    ap.add_argument("--runs", type=int, default=3)
    ap.add_argument("--only", help="comma-separated dataset names")
    ap.add_argument("--rust", default=RUST, help="the Rust build to compare (skipped if missing)")
    args = ap.parse_args()
    rust = [args.rust] if os.path.isfile(args.rust) else None
    os.makedirs(args.data, exist_ok=True)
    names = args.only.split(",") if args.only else list(DATASETS)
    info = system_info(args.rust)
    print(json.dumps(info, indent=1))
    results = []
    for name in names:
        print(f"\n== {name}: {DATASETS[name][0]}", flush=True)
        zp = build_archive(name, args.data)
        out = os.path.join(args.data, "out")
        ref = os.path.join(args.data, "out_7z")
        commands = [("test", "7-Zip", [SEVENZIP, "t", zp], None)]
        impls = [("rust", rust)]
        for label, exe in impls:
            for mode in ("cpu", "gpu", "auto"):
                commands.append(("test", f"{label} {mode}", exe + ["t", zp, "--mode", mode, "--json"], None))
        commands.append(("extract", "7-Zip", [SEVENZIP, "x", zp, f"-o{out}", "-y"], out))
        for label, exe in impls:
            for mode in ("cpu", "auto"):
                commands.append(("extract", f"{label} {mode}", exe + ["x", zp, "-o", out, "--mode", mode, "--json"], out))
        size = None
        times = {i: [] for i in range(len(commands))}
        reports = {}
        # one warm-up round (file cache, CUDA driver caches), then the timed rounds; running the
        # tools in turn inside each round spreads Windows' write throttling evenly over them
        for rnd in range(args.runs + 1):
            for i, (kind, tool, cmd, out_dir) in enumerate(commands):
                s, rep = timed(cmd, out_dir)
                if rnd:
                    times[i].append(s)
                    reports[i] = rep
        shutil.rmtree(ref, ignore_errors=True)
        timed([SEVENZIP, "x", zp, f"-o{ref}", "-y"])  # reference output for the comparison below
        for i, (kind, tool, cmd, out_dir) in enumerate(commands):
            rep = reports[i]
            med = statistics.median(times[i])
            if rep:
                size = rep["bytes"]
            row = {"dataset": name, "kind": kind, "tool": tool, "median_s": round(med, 3),
                   "runs_s": [round(t, 3) for t in times[i]], "zip_bytes": os.path.getsize(zp)}
            if rep:
                row["routes"] = {r: v["files"] for r, v in rep["routes"].items() if v["files"]}
                row["inner_s"] = rep["seconds"]
                if rep.get("gpu"):
                    row["gpu_busy_s"] = round(rep["gpu"]["busy_seconds"], 3)
                    row["gpu_startup_s"] = rep["gpu"]["startup_seconds"]
            results.append(row)
            print(f"  {kind:<8} {tool:<12} {med:7.2f} s   {row.get('routes', '')}", flush=True)
        for row in results:
            if row["dataset"] == name:
                row["bytes"] = size
                row["gbps"] = round(size / row["median_s"] / 1e9, 2)
        same = tree_digest(out) == tree_digest(ref)
        print(f"  {commands[-1][1]} output identical to 7-Zip's: {same}", flush=True)
        for row in results:
            if row["dataset"] == name:
                row["output_matches_7zip"] = same
        shutil.rmtree(out, ignore_errors=True)
        shutil.rmtree(ref, ignore_errors=True)

    with open(os.path.join(HERE, "results.json"), "w") as f:
        json.dump({"system": info, "results": results}, f, indent=1)
    tools = ["7-Zip"] + [f"rust {mode}" for mode in ("cpu", "gpu", "auto")]
    tools = [t for t in tools if any(r["tool"] == t for r in results)]
    lines = [f"System: {info['gpu']} · {info['cpu']} ({info['threads']} threads) · {info['7zip']} · "
             f"nvCOMP {info['nvcomp']} · Python {info['python']}", "",
             "| Dataset | Job | " + " | ".join(t.capitalize() for t in tools) + " | best vs 7-Zip |",
             "|---|---|" + "---|" * (len(tools) + 1)]
    for name in names:
        for kind in ("test", "extract"):
            rows = {r["tool"]: r for r in results if r["dataset"] == name and r["kind"] == kind}
            cells = [f"{rows[t]['median_s']:.2f} s" if t in rows else "-" for t in tools]
            best = min(r["median_s"] for t, r in rows.items() if t != "7-Zip")
            lines.append(f"| {name} | {kind} | " + " | ".join(cells) + f" | {rows['7-Zip']['median_s'] / best:.1f}x |")
    with open(os.path.join(HERE, "results.md"), "w", encoding="utf-8") as f:
        f.write("\n".join(lines) + "\n")
    print("\n" + "\n".join(lines))


if __name__ == "__main__":
    main()
