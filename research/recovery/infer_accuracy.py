"""How often the inferred bytes are right, by threshold: runs gzrescue on a damaged .gz of
realistic.py with several GR_INFER_MARGIN / GR_INFER_ROUNDS settings and counts inferred bytes
that match the original, aligned by the original's true block starts.

    python infer_accuracy.py <realistic-dir> <damaged.gz> <original-plain> [margin:rounds ...]
"""
import json
import os
import subprocess
import sys

import numpy as np

HERE = os.path.dirname(os.path.abspath(__file__))
EXE = os.path.join(HERE, "gzrescue.exe")


def main():
    d, damaged, plain = sys.argv[1:4]
    settings = sys.argv[4:] or ["4.5:40", "8:40", "12:40", "4.5:1", "8:1"]
    orig = np.frombuffer(open(plain, "rb").read(), np.uint8)
    work = os.path.join(d, "_acc")
    os.makedirs(work, exist_ok=True)
    gz_orig = os.path.join(d, os.path.basename(plain) + ".gz") if os.path.exists(os.path.join(d, os.path.basename(plain) + ".gz")) else None
    rp = os.path.join(work, "orig.json")
    subprocess.run([EXE, gz_orig, os.path.join(work, "orig.out"), "--report", rp], capture_output=True)
    starts = {p["in_byte_start"]: p["out_offset"] for p in json.load(open(rp))["pieces"]}
    for s in settings:
        margin, rounds = s.split(":")
        env = dict(os.environ, GR_INFER_MARGIN=margin, GR_INFER_ROUNDS=rounds)
        out, mask, rep = (os.path.join(work, n) for n in ("o", "m", "r.json"))
        subprocess.run([EXE, damaged, out, "--mask", mask, "--report", rep], capture_output=True, env=env)
        g = np.frombuffer(open(out, "rb").read(), np.uint8)
        m = np.frombuffer(open(mask, "rb").read(), np.uint8)
        right = wrong = 0
        for pc in json.load(open(rep))["pieces"]:
            t = starts.get(pc["in_byte_start"])
            if t is None or pc["suspect"]:
                continue
            a, n = pc["out_offset"], min(pc["length"], len(orig) - t)
            seg, sm, rf = g[a:a + n], m[a:a + n], orig[t:t + n]
            inf = sm == 3
            right += int(np.count_nonzero(inf & (seg == rf)))
            wrong += int(np.count_nonzero(inf & (seg != rf)))
        tot = right + wrong
        print(f"margin {margin:>5} rounds {rounds:>3}: inferred {tot:>11,}  right {right:>11,}  wrong {wrong:>11,}  accuracy {100 * right / max(tot, 1):5.1f}%")


main()
