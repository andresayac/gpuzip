"""Checks the inference variables of a damaged .gz against the original: every byte that shares a
variable must have the same true value. Also shows, for wrongly inferred variables, whether the
truth was inconsistent (a mapping error) or the model chose wrong.

    python var_check.py <realistic-dir> <damaged.gz> <original-plain>
"""
import json
import os
import subprocess
import sys
from collections import Counter, defaultdict

import numpy as np

HERE = os.path.dirname(os.path.abspath(__file__))
EXE = os.path.join(HERE, "gzrescue.exe")


def main():
    d, damaged, plain = sys.argv[1:4]
    work = os.path.join(d, "_vc")
    os.makedirs(work, exist_ok=True)
    orig = np.frombuffer(open(plain, "rb").read(), np.uint8)
    rp = os.path.join(work, "orig.json")
    subprocess.run([EXE, plain + ".gz", os.path.join(work, "o.out"), "--report", rp], capture_output=True)
    starts = {p["in_byte_start"]: p["out_offset"] for p in json.load(open(rp))["pieces"]}
    out, mask, rep, vars_ = (os.path.join(work, n) for n in ("x", "m", "r.json", "v.bin"))
    subprocess.run([EXE, damaged, out, "--mask", mask, "--report", rep], capture_output=True,
                   env=dict(os.environ, GR_DUMP_VARS=vars_, GR_INFER_MARGIN="12"))
    g = np.frombuffer(open(out, "rb").read(), np.uint8)
    m = np.frombuffer(open(mask, "rb").read(), np.uint8)
    v = np.frombuffer(open(vars_, "rb").read(), np.uint32)
    truths = defaultdict(Counter)
    chosen = {}
    for pc in json.load(open(rep))["pieces"]:
        t = starts.get(pc["in_byte_start"])
        if t is None or pc["suspect"]:
            continue
        a, n = pc["out_offset"], min(pc["length"], len(orig) - t)
        vv, rf, gg, mm = v[a:a + n], orig[t:t + n], g[a:a + n], m[a:a + n]
        for i in np.nonzero(vv)[0]:
            truths[int(vv[i])][int(rf[i])] += 1
            if mm[i] == 3:
                chosen[int(vv[i])] = int(gg[i])
    inconsistent = sum(1 for c in truths.values() if len(c) > 1)
    print(f"variables {len(truths):,}; with more than one true value: {inconsistent:,}")
    wrong = [(k, c) for k, c in chosen.items() if truths[k].most_common(1)[0][0] != c]
    wrong_incons = sum(1 for k, _ in wrong if len(truths[k]) > 1)
    print(f"inferred variables {len(chosen):,}; wrong {len(wrong):,}, of which the truth was inconsistent: {wrong_incons:,}")
    for k, c in wrong[:10]:
        print("  var", k, "chose", repr(chr(c)), "truth", {repr(chr(b)): n for b, n in truths[k].most_common(4)})


main()
