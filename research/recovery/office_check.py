"""Do Word, Excel and PowerPoint open what `gpuzip rescue` rebuilt from realistic.py's damaged
Office documents? evaluate.py only parses the XML (and loads a workbook with openpyxl), which a
document can pass and still be one the application refuses or repairs.

    python office_check.py <dir-of-realistic.py> [--only report.docx,slides.pptx] [--failures cut,bitrot] [--word-alerts]

Rescues each damaged docx, xlsx and pptx, and opens the rebuilt document (and each undamaged
original, as a control) in its application through COM (office_open.ps1): hidden, alerts off,
read only, one fresh instance per file, two minutes at most. The result is one of clean, repaired
(opened only through the application's own repair), refused, timeout, or none (gpuzip wrote no
document). Windows with Microsoft Office only. Only an instance this script started is ever
ended, found by its window's PID; one the user has open is left alone (PowerPoint keeps a single
instance: a document is then opened in it and closed, nothing else).

Writes office.json and office.md next to the damaged files.
"""
import argparse
import json
import os
import shutil
import subprocess
import sys
import tempfile
import time

HERE = os.path.dirname(os.path.abspath(__file__))
ROOT = os.path.dirname(os.path.dirname(HERE))
GPUZIP = os.path.join(ROOT, "target", "release", "gpuzip.exe")
WORKER = os.path.join(HERE, "office_open.ps1")
APPS = {"docx": ("word", "WINWORD.EXE", "Word"), "xlsx": ("excel", "EXCEL.EXE", "Excel"), "pptx": ("powerpoint", "POWERPNT.EXE", "PowerPoint")}
TIMEOUT = 120


def pids(image):
    r = subprocess.run(["tasklist", "/FI", f"IMAGENAME eq {image}", "/FO", "CSV", "/NH"], capture_output=True, text=True)
    out = set()
    for line in r.stdout.splitlines():
        f = [x.strip('"') for x in line.split('","')]
        if len(f) > 1 and f[1].isdigit():
            out.add(int(f[1]))
    return out


def open_in_office(doc, kind, alerts=False):
    app, image, _ = APPS[kind]
    before = pids(image)
    tmp = tempfile.mkdtemp(prefix="officecheck-")
    pidf, resf = os.path.join(tmp, "pid"), os.path.join(tmp, "result.json")
    cmd = ["powershell.exe", "-NoProfile", "-NonInteractive", "-ExecutionPolicy", "Bypass", "-File", WORKER, "-App", app,
           "-File", os.path.abspath(doc), "-PidFile", pidf, "-ResultFile", resf, "-Preexisting", ",".join(map(str, before))]
    if alerts:
        cmd.append("-Alerts")
    t = time.perf_counter()
    p = subprocess.Popen(cmd, stdout=subprocess.PIPE, stderr=subprocess.STDOUT, text=True, errors="replace")
    try:
        out, _ = p.communicate(timeout=TIMEOUT)
        timed_out = False
    except subprocess.TimeoutExpired:
        p.kill()
        out, _ = p.communicate()
        timed_out = True
    secs = time.perf_counter() - t
    # the instance this script started, if it is still there (a hung open, a Quit that did not end it)
    started = None
    if os.path.exists(pidf):
        pid, attached = open(pidf).read().split()
        if pid != "0" and attached == "0" and int(pid) not in before:
            started = int(pid)
    if started is not None:
        time.sleep(1)
        if started in pids(image):
            subprocess.run(["taskkill", "/PID", str(started), "/F"], capture_output=True)
    if timed_out:
        r = {"result": "timeout"}
    else:
        try:
            r = json.load(open(resf, encoding="utf-8-sig"))
        except (OSError, json.JSONDecodeError):
            r = {"result": "error", "first_error": out[-600:]}
    shutil.rmtree(tmp, ignore_errors=True)
    r["seconds"] = round(secs, 1)
    return r


def rescue(path, work):
    shutil.rmtree(work, ignore_errors=True)
    r = subprocess.run([GPUZIP, "rescue", path, "-o", work, "--json"], capture_output=True, text=True, encoding="utf-8")
    try:
        rep = json.loads(r.stdout.strip().splitlines()[-1])
    except (json.JSONDecodeError, IndexError):
        rep = {}
    return rep.get("office") or {}


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("dir")
    ap.add_argument("--only")
    ap.add_argument("--failures")
    ap.add_argument("--word-alerts", action="store_true", help="Word with alerts on: a prompt blocks the open, reported as a timeout")
    a = ap.parse_args()
    man = json.load(open(os.path.join(a.dir, "manifest.json")))
    only = set(a.only.split(",")) if a.only else None
    fails = set(a.failures.split(",")) if a.failures else None
    rp = os.path.join(a.dir, "office.json")
    results = json.load(open(rp, encoding="utf-8")) if os.path.exists(rp) and (only or fails) else {}
    work = os.path.join(a.dir, "_office")
    docs = [n for n, o in man["originals"].items() if o["kind"] == "office" and (not only or n in only)]
    for name in docs:
        kind = name.rsplit(".", 1)[1]
        alerts = a.word_alerts and kind == "docx"
        r = open_in_office(man["originals"][name]["path"], kind, alerts)
        results[f"{name}|original"] = r
        print(f"{name:12} {'original':14} {r['result']}", flush=True)
        for case in man["cases"]:
            if case["original"] != name or (fails and case["failure"] not in fails):
                continue
            office = rescue(case["path"], os.path.join(work, f"{name}.{case['failure']}"))
            if office.get("rebuilt"):
                r = open_in_office(office["rebuilt"], kind, alerts)
            else:
                r = {"result": "none"}
            r["package_fixes"] = office.get("package_fixes", [])
            results[f"{name}|{case['failure']}"] = r
            err = (r.get("first_error") or "").splitlines()[0][:100] if r.get("first_error") else ""
            print(f"{name:12} {case['failure']:14} {r['result']:9} {json.dumps(r.get('details', {}))[:90]} {err}", flush=True)
    json.dump(results, open(rp, "w", encoding="utf-8"), indent=1, ensure_ascii=False)
    # the table: one column per document
    names = [n for n in man["originals"] if man["originals"][n]["kind"] == "office"]
    failures = list(dict.fromkeys(c["failure"] for c in man["cases"]))
    lines = ["| Failure | " + " | ".join(f"{APPS[n.rsplit('.', 1)[1]][2]}: {n}" for n in names) + " |", "|---|" + "---|" * len(names)]
    for f in ["original"] + failures:
        cells = [results.get(f"{n}|{f}", {}).get("result", "") for n in names]
        lines.append(f"| {f} | " + " | ".join(cells) + " |")
    open(os.path.join(a.dir, "office.md"), "w", encoding="utf-8").write("\n".join(lines) + "\n")
    print("\n".join(lines))


if __name__ == "__main__":
    sys.exit(main())
