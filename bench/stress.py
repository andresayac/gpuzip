"""gpuzip under pressure and on bad input: it must fail cleanly or fall back, never crash or
write wrong data.

  python bench/stress.py --bin PATH --big-gz FILE --big-zip FILE [--cupy-python PATH] [--work DIR]

- the GPU's memory nearly full (another process holds it, through CuPy): gpu and auto modes
  must fall back to the CPU and still be right;
- four gpuzip at once on the GPU;
- a gpuzip killed in the middle of an extraction, then the same extraction again;
- an output folder it may not write to;
- a missing file, a file that is not an archive, an empty file, a ZIP cut in half.
--big-gz: a .gz of a few hundred MB or more; --big-zip: a ZIP with large entries.
"""

import argparse
import json
import os
import shutil
import subprocess
import sys
import tempfile
import time

checks = []


def check(name, ok, detail=""):
    checks.append({"check": name, "ok": bool(ok), "detail": detail})
    print(f"{'ok  ' if ok else 'FAIL'} {name}" + (f": {detail}" if detail else ""), flush=True)


def run(cmd, timeout=900):
    r = subprocess.run(cmd, capture_output=True, text=True, encoding="utf-8", errors="replace", timeout=timeout)
    rep = {}
    for line in reversed(r.stdout.strip().splitlines()):
        try:
            rep = json.loads(line)
            break
        except json.JSONDecodeError:
            continue
    return r.returncode, rep, r.stdout + r.stderr


def clean(text):
    return "panicked" not in text and "RUST_BACKTRACE" not in text


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--bin", required=True)
    ap.add_argument("--big-gz", required=True)
    ap.add_argument("--big-zip", required=True)
    ap.add_argument("--cupy-python", help="a Python with CuPy, to fill the GPU's memory")
    ap.add_argument("--work", default=os.path.join(tempfile.gettempdir(), "gpuzip-bench", "stress"))
    a = ap.parse_args()
    g = a.bin
    os.makedirs(a.work, exist_ok=True)

    # the GPU's memory nearly full
    if a.cupy_python:
        hog = subprocess.Popen([a.cupy_python, "-c", (
            "import cupy as cp, sys, time\n"
            "free, total = cp.cuda.runtime.memGetInfo()\n"
            "x = cp.empty(int(free - (300 << 20)), dtype=cp.uint8)\n"
            "print('holding', x.nbytes >> 20, 'MB of', total >> 20, flush=True)\n"
            "sys.stdin.read()\n")], stdin=subprocess.PIPE, stdout=subprocess.PIPE, text=True)
        held = hog.stdout.readline().strip()
        for mode in ("gpu", "auto"):
            rc, rep, out = run([g, "t", a.big_gz, "--mode", mode, "--json"])
            check(f"GPU memory full ({held}): t {os.path.basename(a.big_gz)} --mode {mode}",
                  rc == 0 and clean(out), f"routes {rep.get('routes')}, notes {rep.get('notes')}")
            rc, rep, out = run([g, "t", a.big_zip, "--mode", mode, "--json"])
            check(f"GPU memory full: t {os.path.basename(a.big_zip)} --mode {mode}",
                  rc == 0 and clean(out), f"routes {rep.get('routes')}, notes {(rep.get('notes') or [])[:2]}")
        hog.stdin.close()
        hog.wait(timeout=60)

    # four at once on the GPU
    procs = [subprocess.Popen([g, "t", a.big_gz, "--mode", "gpu", "--json"], stdout=subprocess.PIPE,
                              stderr=subprocess.STDOUT, text=True, encoding="utf-8", errors="replace") for _ in range(4)]
    results = [(p.wait(timeout=900), p.stdout.read()) for p in procs]
    check("four gpuzip at once on the GPU", all(rc == 0 and clean(o) for rc, o in results),
          f"exit codes {[rc for rc, _ in results]}")

    # killed in the middle of an extraction, then again
    out_dir = os.path.join(a.work, "killed")
    shutil.rmtree(out_dir, ignore_errors=True)
    p = subprocess.Popen([g, "x", a.big_zip, "-o", out_dir, "-q"])
    time.sleep(2.5)
    p.kill()
    p.wait()
    rc, rep, out = run([g, "x", a.big_zip, "-o", out_dir, "--json"])
    check("extraction killed after 2.5 s, then run again into the same folder", rc == 0 and not rep.get("failed") and clean(out),
          f"exit {rc}, {rep.get('files')} files")
    rc2, rep2, out2 = run([g, "t", a.big_zip, "--json"])
    shutil.rmtree(out_dir, ignore_errors=True)

    # an output folder it may not write to
    denied = os.path.join(os.environ.get("SystemRoot", "C:\\Windows"), "System32", "gpuzip-stress-test") if os.name == "nt" else "/proc/gpuzip"
    rc, rep, out = run([g, "x", a.big_gz, "-o", denied, "--json"])
    check("output folder it may not write to", rc != 0 and clean(out) and not os.path.exists(denied), f"exit {rc}: {out.strip()[-160:]}")

    # bad input
    bad = os.path.join(a.work, "bad")
    shutil.rmtree(bad, ignore_errors=True)
    os.makedirs(bad)
    open(os.path.join(bad, "notazip.zip"), "w").write("this is not an archive\n" * 100)
    open(os.path.join(bad, "empty.zip"), "wb").close()
    open(os.path.join(bad, "empty.gz"), "wb").close()
    data = open(a.big_zip, "rb").read(64 << 20)
    open(os.path.join(bad, "cut.zip"), "wb").write(data[: len(data) // 2])
    gzdata = open(a.big_gz, "rb").read(32 << 20)
    open(os.path.join(bad, "cut.gz"), "wb").write(gzdata[: len(gzdata) // 2])
    for name, cmd in (("missing file", "x"), ("notazip.zip", "x"), ("empty.zip", "x"), ("empty.gz", "x"),
                      ("cut.zip", "x"), ("cut.gz", "t"), ("cut.zip", "rescue")):
        path = os.path.join(bad, name) if name != "missing file" else os.path.join(bad, "does-not-exist.zip")
        rc, rep, out = run([g, cmd, path, "-o", os.path.join(bad, "out"), "--json"] if cmd != "t" else [g, cmd, path, "--json"])
        check(f"{cmd} on {name}: a clean error, no crash", rc in (1, 2) and clean(out), f"exit {rc}: {out.strip()[-140:]}")
    shutil.rmtree(bad, ignore_errors=True)

    path = os.path.join(a.work, "stress.json")
    json.dump(checks, open(path, "w"), indent=1)
    failed = [c for c in checks if not c["ok"]]
    print(f"\n{len(checks) - len(failed)} of {len(checks)} ok; {path}")
    return 1 if failed else 0


if __name__ == "__main__":
    sys.exit(main())
