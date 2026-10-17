"""Shows wrong inference decisions with their contexts and candidate scores.

    python infer_debug.py <realistic-dir> <damaged.gz> <original-plain> [margin=12] [rounds=1]
"""
import json
import os
import subprocess
import sys
from collections import Counter, defaultdict

import numpy as np

HERE = os.path.dirname(os.path.abspath(__file__))
EXE = os.path.join(HERE, "gzrescue.exe")


def show(hexs):
    b = bytes.fromhex(hexs)
    return "".join(chr(x) if 32 <= x < 127 else f"\\x{x:02x}" for x in b)


def main():
    d, damaged, plain = sys.argv[1:4]
    margin = sys.argv[4] if len(sys.argv) > 4 else "12"
    rounds = sys.argv[5] if len(sys.argv) > 5 else "1"
    work = os.path.join(d, "_dbg")
    os.makedirs(work, exist_ok=True)
    orig = np.frombuffer(open(plain, "rb").read(), np.uint8)
    rp = os.path.join(work, "orig.json")
    subprocess.run([EXE, plain + ".gz", os.path.join(work, "o.out"), "--report", rp], capture_output=True)
    starts = {p["in_byte_start"]: p["out_offset"] for p in json.load(open(rp))["pieces"]}
    out, rep, vars_, log = (os.path.join(work, n) for n in ("x", "r.json", "v.bin", "dbg.txt"))
    subprocess.run([EXE, damaged, out, "--report", rep], capture_output=True,
                   env=dict(os.environ, GR_DUMP_VARS=vars_, GR_INFER_MARGIN=margin, GR_INFER_ROUNDS=rounds, GR_INFER_DEBUG=log))
    v = np.frombuffer(open(vars_, "rb").read(), np.uint32)
    truths = defaultdict(Counter)
    for pc in json.load(open(rep))["pieces"]:
        t = starts.get(pc["in_byte_start"])
        if t is None or pc["suspect"]:
            continue
        a, n = pc["out_offset"], min(pc["length"], len(orig) - t)
        vv, rf = v[a:a + n], orig[t:t + n]
        for i in np.nonzero(vv)[0]:
            truths[int(vv[i])][int(rf[i])] += 1
    wrong = right = 0
    shown = 0
    for line in open(log):
        head, ctxs, tops = line.rstrip("\n").split("|")
        f = head.split()
        var, chose = int(f[1]), int(f[2])
        if var not in truths:
            continue
        t = truths[var].most_common(1)[0][0]
        if t == chose:
            right += 1
            continue
        wrong += 1
        if shown < 15:
            shown += 1
            cs = [c.strip("[]").split("_") for c in ctxs.split()]
            print(f"var {var}: chose {chr(chose)!r}, truth {chr(t)!r}; margin {float(f[3]) - float(f[4]):.1f}, "
                  f"{f[5]} candidates, {f[6]} contexts; top {tops.strip()}")
            for before, after in cs:
                print(f"    {show(before)!r} [?] {show(after)!r}")
    print(f"round decisions with truth: right {right}, wrong {wrong}")


main()
