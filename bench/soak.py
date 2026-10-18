"""gpuzip over time, beside a busy GPU, and interrupted.

  python bench/soak.py --bin PATH --gz FILE --zip FILE --big-zip FILE [--runs 200] [--cupy-python PATH]

- --runs runs in a row on the GPU, alternating a .gz (gpuinflate) and a ZIP of many small entries
  (nvCOMP): every run right, its time steady, the GPU's memory back where it was after each;
- the same while another process keeps the GPU busy with matrix products (CuPy);
- an extraction of --big-zip stopped with Ctrl+C, and one killed, after 2 s: what is left in the
  output folder, and whether a second extraction into it comes out whole.
"""

import argparse
import json
import os
import shutil
import signal
import statistics
import subprocess
import sys
import tempfile
import time

results = []


def record(name, ok, detail):
    results.append({"check": name, "ok": ok, "detail": detail})
    print(f"{'ok  ' if ok else 'FAIL' if ok is False else 'info'} {name}: {detail}", flush=True)


def gpu_memory_mb():
    try:
        out = subprocess.run(["nvidia-smi", "--query-gpu=memory.used", "--format=csv,noheader,nounits"], capture_output=True,
                             text=True, timeout=20).stdout
        return int(out.strip().splitlines()[0])
    except Exception:  # noqa: BLE001
        return -1


def t(exe, path, mode="gpu"):
    t0 = time.perf_counter()
    r = subprocess.run([exe, "t", path, "--mode", mode, "--json"], capture_output=True, text=True, encoding="utf-8",
                       errors="replace")
    rep = json.loads(r.stdout.strip().splitlines()[-1]) if r.stdout.strip() else {}
    return r.returncode, time.perf_counter() - t0, rep


def runs(exe, files, n, label):
    times = {f: [] for f in files}
    mem = []
    bad = 0
    gpu_used = 0
    for i in range(n):
        f = files[i % len(files)]
        rc, secs, rep = t(exe, f)
        times[f].append(secs)
        bad += rc != 0
        gpu_used += (rep.get("routes", {}).get("gpu", {}).get("files", 0) > 0)
        if i % 10 == 0:
            mem.append(gpu_memory_mb())
    for f, ts in times.items():
        first, last = statistics.median(ts[:10]), statistics.median(ts[-10:])
        record(f"{label}: {os.path.basename(f)} x{len(ts)}", bad == 0 and last < first * 1.5,
               f"median {statistics.median(ts):.3f} s, first ten {first:.3f} s, last ten {last:.3f} s, max {max(ts):.3f} s")
    record(f"{label}: all right, on the GPU", bad == 0 and gpu_used == n, f"{n - bad} of {n} exit 0, {gpu_used} used the GPU")
    if mem and mem[0] >= 0:
        record(f"{label}: GPU memory between runs", max(mem) - min(mem) < 200, f"{min(mem)}-{max(mem)} MB used (sampled every 10 runs)")


def interrupt(exe, big_zip, work, how):
    out = os.path.join(work, f"interrupted-{how}")
    shutil.rmtree(out, ignore_errors=True)
    flags = subprocess.CREATE_NEW_PROCESS_GROUP if os.name == "nt" else 0
    p = subprocess.Popen([exe, "x", big_zip, "-o", out, "-q"], creationflags=flags)
    time.sleep(2)
    if how == "ctrl-c":
        p.send_signal(signal.CTRL_BREAK_EVENT if os.name == "nt" else signal.SIGINT)
    else:
        p.kill()
    rc = p.wait(timeout=120)
    left = []
    for dp, _, fs in os.walk(out):
        for f in fs:
            left.append((os.path.relpath(os.path.join(dp, f), out), os.path.getsize(os.path.join(dp, f))))
    import zipfile
    sizes = {i.filename: i.file_size for i in zipfile.ZipFile(big_zip).infolist()}
    partial = [(n, s, sizes.get(n.replace("\\", "/"))) for n, s in left if sizes.get(n.replace("\\", "/")) not in (None, s)]
    record(f"extraction stopped by {how} after 2 s", None,
           f"exit {rc}; {len(left)} files left, {len(partial)} of them partial with their final names: {partial[:3]}")
    r = subprocess.run([exe, "x", big_zip, "-o", out, "--json"], capture_output=True, text=True, encoding="utf-8", errors="replace")
    rep = json.loads(r.stdout.strip().splitlines()[-1])
    record(f"after {how}: the same extraction again comes out whole", r.returncode == 0 and not rep.get("failed"),
           f"exit {r.returncode}, {rep.get('files')} files")
    shutil.rmtree(out, ignore_errors=True)


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--bin", required=True)
    ap.add_argument("--gz", required=True)
    ap.add_argument("--zip", required=True)
    ap.add_argument("--big-zip", required=True)
    ap.add_argument("--runs", type=int, default=200)
    ap.add_argument("--cupy-python")
    ap.add_argument("--work", default=os.path.join(tempfile.gettempdir(), "gpuzip-bench", "soak"))
    a = ap.parse_args()
    os.makedirs(a.work, exist_ok=True)
    exe = os.path.abspath(a.bin)

    runs(exe, [a.gz, a.zip], a.runs, "in a row")
    if a.cupy_python:
        busy = subprocess.Popen([a.cupy_python, "-c", (
            "import cupy as cp\n"
            "x = cp.random.rand(8192, 8192, dtype=cp.float32)\n"
            "while True:\n"
            "    y = x @ x\n"
            "    cp.cuda.Stream.null.synchronize()\n")])
        time.sleep(5)
        runs(exe, [a.gz, a.zip], 20, "GPU busy with another process")
        busy.kill()
        busy.wait()
    for how in ("ctrl-c", "kill"):
        interrupt(exe, a.big_zip, a.work, how)

    path = os.path.join(a.work, "soak.json")
    json.dump(results, open(path, "w"), indent=1)
    bad = [r for r in results if r["ok"] is False]
    print(f"\n{sum(1 for r in results if r['ok'])} ok, {len(bad)} failed; {path}")
    return 1 if bad else 0


if __name__ == "__main__":
    sys.exit(main())
