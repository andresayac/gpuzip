"""Scores `gpuzip rescue` on the damaged files of realistic.py against what people would use.

    python evaluate.py <dir-of-realistic.py> [--only code.tar.gz,lib.zip] [--failures cut,bitrot]
    (a partial run updates results.json from an earlier full one)

For every damaged file:
  .gz       bytes right: gzip -d (Git for Windows) up to its first wrong byte, vs gpuzip's bytes
            equal to the original at their place; gpuzip's silent errors (bytes it marks exact
            that are wrong) from gzrescue's mask
  .tar.gz   files identical to the original: Windows tar, 7-Zip, gpuzip; gpuzip's silent errors
            (files it calls intact that differ)
  .zip      entries identical: 7-Zip, Python zipfile (entry by entry), gpuzip; silent errors
  Office    the same for the parts, plus the share of the document's text units (paragraphs,
            cells, slide texts) found in the recovered XML, and whether the result opens: the
            damaged file itself for the others (zipfile + every XML part parses; openpyxl for
            xlsx), gpuzip's rebuilt document for gpuzip
Writes results.json and results.md next to the damaged files.
"""
import argparse
import hashlib
import html
import io
import json
import os
import re
import shutil
import subprocess
import tarfile
import time
import xml.etree.ElementTree as ET
import sys
import zipfile

HERE = os.path.dirname(os.path.abspath(__file__))
sys.path.insert(0, HERE)
ROOT = os.path.dirname(os.path.dirname(HERE))
GPUZIP = os.path.join(ROOT, "rust", "target", "release", "gpuzip.exe")
GZRESCUE = os.path.join(HERE, "gzrescue.exe")
SEVENZIP = r"C:\Program Files\7-Zip\7z.exe"
GZIP = r"C:\Program Files\Git\usr\bin\gzip.exe"
TAR = r"C:\Windows\System32\tar.exe"


def sha(b):
    return hashlib.sha256(b).hexdigest()


def tree_hashes(root):
    out = {}
    for d, _, files in os.walk(root):
        for n in files:
            p = os.path.join(d, n)
            out[os.path.relpath(p, root).replace("\\", "/")] = sha(open(p, "rb").read())
    return out


def run(cmd, **kw):
    return subprocess.run(cmd, capture_output=True, **kw)


def gpuzip_rescue(path, out):
    shutil.rmtree(out, ignore_errors=True)
    t = time.perf_counter()
    r = run([GPUZIP, "rescue", path, "-o", out, "--json"], text=True, encoding="utf-8")
    secs = time.perf_counter() - t
    try:
        rep = json.loads(r.stdout.strip().splitlines()[-1])
    except (json.JSONDecodeError, IndexError):
        rep = {"error": (r.stdout + r.stderr)[-400:]}
    return rep, secs


def right_prefix(a, b):
    """how much of a is b's start: gzip's output up to its first wrong byte. After it gzip either
    stopped or decodes garbage, and in a repetitive log that garbage agrees with the original
    here and there by chance (0.9% of the crash case): nothing a user could tell apart"""
    n = min(len(a), len(b))
    if n == 0:
        return 0
    import numpy as np
    x = np.frombuffer(a[:n], dtype=np.uint8)
    y = np.frombuffer(b[:n], dtype=np.uint8)
    ne = np.nonzero(x != y)[0]
    return int(ne[0]) if len(ne) else n


def xml_text(blobs):
    """the text content of XML parts, unescaped, tags removed"""
    out = []
    for b in blobs:
        s = b.decode("utf-8", "replace")
        out.append(html.unescape(re.sub(r"<[^>]*>", " ", s)))
    return " ".join(out)


def opens(path, kind):
    """would an Office application read this? zipfile, every XML part parses (openpyxl for xlsx)"""
    try:
        with zipfile.ZipFile(path) as z:
            for n in z.namelist():
                if n.endswith(".xml") or n.endswith(".rels"):
                    ET.fromstring(z.read(n))
                else:
                    z.read(n)
        if kind == "xlsx":
            from openpyxl import load_workbook
            load_workbook(path)
        return True
    except Exception:  # noqa: BLE001
        return False


_TRUE_STARTS = {}


def true_block_offsets(gz_path, work):
    """block start (input byte) -> output offset, from rescuing the undamaged original"""
    if gz_path not in _TRUE_STARTS:
        os.makedirs(work, exist_ok=True)
        rp = os.path.join(work, "orig.json")
        run([GZRESCUE, gz_path, os.path.join(work, "orig.out"), "--report", rp])
        _TRUE_STARTS[gz_path] = {p["in_byte_start"]: p["out_offset"] for p in json.load(open(rp))["pieces"]}
    return _TRUE_STARTS[gz_path]


def to_original_input(case, orig_gz):
    """maps an input byte of the damaged file to the same byte of the original"""
    f, w = case["failure"], case["what"]
    if f == "resume_dup":
        p, dup = w["duplicated"]
        return lambda x: x if x < p + dup else x - dup
    if f == "resume_gap":
        p, gap = w["missing"]
        return lambda x: x if x < p else x + gap
    if f == "ascii_strip":
        import bisect
        data = open(orig_gz, "rb").read()
        removed, k = [], 0  # where each removed CR sits, in the damaged file's coordinates
        i = data.find(b"\r\n")
        while i >= 0:
            removed.append(i - k)
            k += 1
            i = data.find(b"\r\n", i + 1)
        return lambda x: x + bisect.bisect_right(removed, x)
    return lambda x: x


def eval_gz(case, o, work):
    import numpy as np
    orig = open(o["plain"], "rb").read()
    r = run([GZIP, "-dc", case["path"]])
    base = right_prefix(r.stdout, orig)
    rep, secs = gpuzip_rescue(case["path"], work)
    # gpuzip's bytes piece by piece, each compared where it belongs in the original: a piece that
    # starts at a true block boundary goes where the original's block does; the library's own
    # output, mask and report, the same code gpuzip runs
    os.makedirs(work, exist_ok=True)
    out_p, mask_p, rep_p = (os.path.join(work, n) for n in ("lib.out", "lib.mask", "lib.json"))
    run([GZRESCUE, case["path"], out_p, "--mask", mask_p, "--report", rep_p])
    exact = silent = warned = inferred = inferred_wrong = 0
    if os.path.exists(rep_p):
        starts = true_block_offsets(o["path"], work)
        mp = to_original_input(case, o["path"])
        ref = np.frombuffer(orig, dtype=np.uint8)
        g = np.frombuffer(open(out_p, "rb").read(), dtype=np.uint8)
        m = np.frombuffer(open(mask_p, "rb").read(), dtype=np.uint8)
        covered = np.zeros(len(orig), dtype=bool)
        for pc in json.load(open(rep_p))["pieces"]:
            t = starts.get(mp(pc["in_byte_start"]))
            if t is None:
                continue  # a restart inside a block: no place to compare it with; not counted
            a, n = pc["out_offset"], min(pc["length"], len(orig) - t)
            seg, sm, rf = g[a:a + n], m[a:a + n], ref[t:t + n]
            n = min(len(seg), len(rf))
            seg, sm, rf = seg[:n], sm[:n], rf[:n]
            fresh = ~covered[t:t + n]  # a duplicated stretch is counted once
            eq = (seg == rf) & fresh
            exact += int(np.count_nonzero(eq & (sm != 0)))
            inferred += int(np.count_nonzero(eq & (sm == 3)))
            silent += int(np.count_nonzero(~eq & fresh & (sm == 1)))
            warned += int(np.count_nonzero(~eq & fresh & (sm == 2)))
            inferred_wrong += int(np.count_nonzero(~eq & fresh & (sm == 3)))
            covered[t:t + n] = True
    return {"baseline": {"gzip": 100 * base / len(orig)}, "gpuzip": 100 * exact / len(orig), "silent": silent,
            "warned_wrong": warned, "inferred_right": inferred, "inferred_wrong": inferred_wrong, "unit": "% bytes", "seconds": secs, "report": summarize(rep)}


def eval_tgz(case, o, work):
    with tarfile.open(o["tar"]) as t:
        members = {m.name: sha(t.extractfile(m).read()) for m in t if m.isfile()}
    def count(root):
        h = tree_hashes(root) if os.path.isdir(root) else {}
        return sum(1 for k, v in members.items() if h.get(k) == v)
    res = {}
    d = work + "-tar"
    shutil.rmtree(d, ignore_errors=True)
    os.makedirs(d)
    run([TAR, "-xzf", case["path"], "-C", d])
    res["Windows tar"] = count(d)
    d = work + "-7z"
    shutil.rmtree(d, ignore_errors=True)
    subprocess.run(f'"{SEVENZIP}" x -so "{case["path"]}" 2>nul | "{SEVENZIP}" x -si -ttar -y "-o{d}"', shell=True, capture_output=True)
    res["7-Zip"] = count(d)
    rep, secs = gpuzip_rescue(case["path"], work)
    got = tree_hashes(work) if os.path.isdir(work) else {}
    ours = sum(1 for k, v in members.items() if got.get(k) == v)
    tar = rep.get("tar") or {}
    inferred = set(tar.get("inferred", []))
    flagged = set(tar.get("suspect", [])) | set(tar.get("partial", [])) | inferred
    unverified = tar.get("unverified", 0)
    silent = 0 if unverified else sum(1 for k, v in members.items() if k in got and got[k] != v and k not in flagged)
    # files called complete with inferred bytes: a guess that was wrong is not silent, but counted
    inferred_wrong = sum(1 for k in inferred if got.get(k) != members.get(k))
    return {"baseline": res, "gpuzip": ours, "of": len(members), "silent": silent, "inferred_wrong": inferred_wrong,
            "unit": "files", "seconds": secs, "report": summarize(rep)}


def zip_members(path):
    with zipfile.ZipFile(path) as z:
        return {i.filename: sha(z.read(i)) for i in z.infolist() if not i.is_dir()}


def zipfile_baseline(path, members):
    good = 0
    try:
        with zipfile.ZipFile(path) as z:
            for i in z.infolist():
                try:
                    if members.get(i.filename) == sha(z.read(i)):
                        good += 1
                except Exception:  # noqa: BLE001
                    pass
    except Exception:  # noqa: BLE001
        pass
    return good


def eval_zip(case, o, work, office=False):
    members = zip_members(o["path"])
    res = {}
    d = work + "-7z"
    shutil.rmtree(d, ignore_errors=True)
    run([SEVENZIP, "x", case["path"], f"-o{d}", "-y"])
    h7 = tree_hashes(d) if os.path.isdir(d) else {}
    res["7-Zip"] = sum(1 for k, v in members.items() if h7.get(k) == v)
    res["zipfile"] = zipfile_baseline(case["path"], members)
    rep, secs = gpuzip_rescue(case["path"], work)
    got = tree_hashes(work) if os.path.isdir(work) else {}
    ours = sum(1 for k, v in members.items() if got.get(k) == v)
    status = {e["name"]: e["status"] for e in rep.get("entries", [])}
    silent = sum(1 for k, v in members.items() if status.get(k) == "intact" and got.get(k) != v)
    out = {"baseline": res, "gpuzip": ours, "of": len(members), "silent": silent, "unit": "entries",
           "seconds": secs, "report": summarize(rep)}
    if office:
        kind = o["path"].rsplit(".", 1)[1]
        units = o["units"]
        def parts(root):
            blobs = []
            if os.path.isdir(root):
                for dd, _, files in os.walk(root):
                    for n in files:
                        if n.endswith(".xml"):
                            blobs.append(open(os.path.join(dd, n), "rb").read())
            return blobs
        t7 = xml_text(parts(d))
        tg = xml_text(parts(work))
        out["text_7z"] = 100 * sum(1 for u in units if u in t7) / len(units)
        out["text_gpuzip"] = 100 * sum(1 for u in units if u in tg) / len(units)
        out["opens_damaged"] = opens(case["path"], kind)
        rebuilt = (rep.get("office") or {}).get("rebuilt")
        out["opens_rebuilt"] = bool(rebuilt) and opens(rebuilt, kind)
    return out


def summarize(rep):
    if "error" in rep:
        return {"error": rep["error"]}
    keys = ("clean", "damaged_blocks", "crc_check", "counts", "directory_ok", "notes")
    s = {k: rep[k] for k in keys if k in rep}
    if rep.get("tar"):
        s["tar"] = {k: (len(v) if isinstance(v, list) else v) for k, v in rep["tar"].items()}
    return s


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("dir")
    ap.add_argument("--only")
    ap.add_argument("--failures")
    a = ap.parse_args()
    man = json.load(open(os.path.join(a.dir, "manifest.json")))
    only = set(a.only.split(",")) if a.only else None
    fails = set(a.failures.split(",")) if a.failures else None
    work = os.path.join(a.dir, "_work")
    results = []
    for case in man["cases"]:
        if (only and case["original"] not in only) or (fails and case["failure"] not in fails):
            continue
        o = man["originals"][case["original"]]
        k = o["kind"]
        if k == "gz":
            r = eval_gz(case, o, work)
        elif k == "tgz":
            r = eval_tgz(case, o, work)
        else:
            r = eval_zip(case, o, work, office=k == "office")
        r.update({"original": case["original"], "failure": case["failure"]})
        results.append(r)
        of = f" of {r['of']}" if "of" in r else ""
        base = ", ".join(f"{n} {v:.1f}" if isinstance(v, float) else f"{n} {v}" for n, v in r["baseline"].items())
        g = f"{r['gpuzip']:.1f}" if isinstance(r["gpuzip"], float) else r["gpuzip"]
        extra = ""
        if "inferred_right" in r:
            extra = (f" · inferred right {r['inferred_right']:,}, inferred wrong {r['inferred_wrong']:,},"
                     f" suspect bytes wrong {r['warned_wrong']:,}")
        elif "inferred_wrong" in r:
            extra = f" · files with inferred bytes that are wrong {r['inferred_wrong']}"
        if "text_gpuzip" in r:
            extra = (f" · text {r['text_7z']:.0f}% (7-Zip) vs {r['text_gpuzip']:.0f}% · damaged file opens {r['opens_damaged']},"
                     f" rebuilt opens {r['opens_rebuilt']}")
        print(f"{case['original']:12} {case['failure']:14} {r['unit']}{of}: {base} | gpuzip {g} (silent errors {r['silent']}, {r['seconds']:.1f} s){extra}",
              flush=True)
    # a partial run (--only, --failures) updates the results of a full one instead of replacing them
    rp = os.path.join(a.dir, "results.json")
    if (only or fails) and os.path.exists(rp):
        done = {(r["original"], r["failure"]): r for r in json.load(open(rp))}
        done.update({(r["original"], r["failure"]): r for r in results})
        order = [(c["original"], c["failure"]) for c in man["cases"]]
        results = [done[k] for k in order if k in done]
    json.dump(results, open(rp, "w"), indent=1)
    lines = []
    for orig in dict.fromkeys(r["original"] for r in results):
        rows = [r for r in results if r["original"] == orig]
        first = rows[0]
        names = list(first["baseline"])
        of = f" (of {first['of']})" if "of" in first else ""
        office = "text_gpuzip" in first
        lines.append(f"\n**{orig}**: {first['unit']}{of}\n")
        guessed = "inferred_wrong" in first
        head = "| Failure | " + " | ".join(names) + " | gpuzip rescue | silent errors |" + (" inferred, wrong |" if guessed else "")
        sep = "|---|" + "---|" * (len(names) + 2 + guessed)
        if office:
            head += " text: 7-Zip | text: gpuzip | damaged file opens | rebuilt opens |"
            sep += "---|---|---|---|"
        lines += [head, sep]
        for r in rows:
            fmt = lambda v: f"{v:.1f}%" if isinstance(v, float) else str(v)  # noqa: E731
            row = f"| {r['failure']} | " + " | ".join(fmt(r["baseline"][n]) for n in names) + f" | **{fmt(r['gpuzip'])}** | {r['silent']} |"
            if guessed:
                row += f" {r['inferred_wrong']:,} |"
            if office:
                row += f" {r['text_7z']:.0f}% | **{r['text_gpuzip']:.0f}%** | {'yes' if r['opens_damaged'] else 'no'} | {'yes' if r['opens_rebuilt'] else 'no'} |"
            lines.append(row)
    open(os.path.join(a.dir, "results.md"), "w", encoding="utf-8").write("\n".join(lines) + "\n")
    print("\n".join(lines))


if __name__ == "__main__":
    main()
