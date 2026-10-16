"""Measures what gzrescue recovers from the damaged files damage.py made, against the original,
and against what gzip gives (everything before the first error).

    python measure.py <dir-with-damage.json> [--rescue path\\to\\gzrescue.exe]

For every case: bytes recovered exactly, bytes marked uncertain, bytes marked good but wrong
(silent errors, the number that must be zero), bytes lost; and for tar originals, how many of
the archive's files came back intact.
"""
import argparse
import io
import json
import os
import subprocess
import tarfile
import zlib

import numpy as np

HERE = os.path.dirname(os.path.abspath(__file__))


def gzip_gives(path):
    """bytes a standard decoder produces before it fails (gzip -d keeps these)"""
    data = open(path, "rb").read()
    # skip the header (FNAME)
    p = 10
    if data[3] & 8:
        p = data.index(b"\0", 10) + 1
    d = zlib.decompressobj(-15)
    out = bytearray()
    try:
        for i in range(p, len(data), 65536):
            out += d.decompress(data[i:i + 65536])
        out += d.flush()
    except zlib.error:
        pass
    return bytes(out)


def locate(original, piece, known_mask, expected):
    """the piece's true offset in the original: among the places where 256 known bytes of it
    occur near the offset the report gives, the one where the most of the piece matches
    (repetitive data has the same bytes in many places)"""
    orig = np.frombuffer(original, dtype=np.uint8)
    pa = np.frombuffer(piece, dtype=np.uint8)
    n = len(piece)
    candidates = set()
    anchors = 0
    for start in range(0, n - 256, 4096):
        if not known_mask[start:start + 256].all():
            continue
        anchor = piece[start:start + 256]
        lo, hi = max(0, expected + start - (8 << 20)), expected + start + (8 << 20)
        at = original.find(anchor, lo)
        found = 0
        while 0 <= at <= hi and found < 64:
            if at - start >= 0:
                candidates.add(at - start)
            found += 1
            at = original.find(anchor, at + 1)
        if not found:
            at = original.find(anchor)
            if at - start >= 0:
                candidates.add(at - start)
        anchors += 1
        if anchors >= 3 and candidates:
            break
    best, best_score = None, -1
    for c in candidates:
        ref = orig[c:c + n]
        score = int(np.count_nonzero(pa[: len(ref)] == ref))
        if score > best_score:
            best, best_score = c, score
    return best


def evaluate(original, out, mask, report):
    """(exact, uncertain, wrong, suspect_wrong, lost, piece offsets in the original)

    mask: per output byte, 1 exact, 2 in a damaged block, 0 not known.
    exact: marked 1 or 2 and equal to the original; wrong: marked 1 and different (a silent error);
    suspect_wrong: marked 2 and different (the tool warned); uncertain: marked 0 inside a piece."""
    orig = np.frombuffer(original, dtype=np.uint8)
    outa = np.frombuffer(out, dtype=np.uint8)
    maska = np.frombuffer(mask, dtype=np.uint8)
    exact = uncertain = wrong = suspect_wrong = 0
    covered = np.zeros(len(original), dtype=bool)
    offsets = []
    for pc in report["pieces"]:
        o, n = pc["out_offset"], pc["length"]
        piece, pmask = outa[o:o + n], maska[o:o + n]
        true_at = o
        ref = orig[o:o + n]
        # a piece that does not sit where the report says (an earlier damaged block's length was
        # misjudged): find it by its first exact bytes
        if len(ref) < n or not np.array_equal(piece[pmask == 1], ref[pmask == 1]):
            found = locate(original, out[o:o + n], pmask != 0, o)
            if found is not None:
                true_at = found
                ref = orig[true_at:true_at + n]
        if len(ref) < n:
            ref = np.concatenate([ref, np.zeros(n - len(ref), dtype=np.uint8)])
        eq = piece == ref
        uncertain += int(np.count_nonzero(pmask == 0))
        exact += int(np.count_nonzero(eq & (pmask != 0)))
        wrong += int(np.count_nonzero(~eq & (pmask == 1)))
        suspect_wrong += int(np.count_nonzero(~eq & (pmask == 2)))
        covered[true_at:min(len(original), true_at + n)] = True
        offsets.append(true_at)
    lost = len(original) - int(np.count_nonzero(covered))
    return exact, uncertain, wrong, suspect_wrong, lost, offsets


def tar_members(original):
    try:
        t = tarfile.open(fileobj=io.BytesIO(original))
    except tarfile.TarError:
        return None
    members = []
    for m in t:
        if m.isfile():
            members.append((m.name, m.offset_data, m.size))
    return members


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("dir")
    ap.add_argument("--rescue", default=os.path.join(HERE, "gzrescue.exe"))
    a = ap.parse_args()
    meta = json.load(open(os.path.join(a.dir, "damage.json")))
    original = open(meta["original"], "rb").read()
    name = os.path.basename(meta["original"])
    members = tar_members(original)
    print(f"{name}: {len(original)} bytes" + (f", a tar of {len(members)} files" if members else ""))
    print(f"{'case':8} {'gzip gives':>11} {'rescued exact':>14} {'uncertain':>10} {'lost':>9} {'silent errors':>14} {'warned wrong':>13} {'pieces':>7}"
          + (f" {'files intact':>13}" if members else ""))
    for case in meta["cases"]:
        path = os.path.join(a.dir, f"{name}.{case}.gz")
        base = gzip_gives(path)
        base_ok = 0
        for i, (x, y) in enumerate(zip(base, original)):
            if x != y:
                break
            base_ok = i + 1
        out_path = os.path.join(a.dir, f"{name}.{case}.rescued")
        rep_path, mask_path = out_path + ".json", out_path + ".mask"
        r = subprocess.run([a.rescue, path, out_path, "--report", rep_path, "--mask", mask_path], capture_output=True, text=True)
        if r.returncode != 0:
            print(f"{case:8} gzrescue failed: {r.stderr.strip()}")
            continue
        report = json.load(open(rep_path))
        out = open(out_path, "rb").read()
        mask = open(mask_path, "rb").read()
        exact, uncertain, wrong, suspect_wrong, lost, offsets = evaluate(original, out, mask, report)
        n = len(original)
        line = (f"{case:8} {100 * base_ok / n:10.2f}% {100 * exact / n:13.3f}% {100 * uncertain / n:9.3f}% {100 * lost / n:8.3f}%"
                f" {wrong:14} {suspect_wrong:13} {len(report['pieces']):7}")
        crc = [m["crc_check"] for m in report.get("members", [])]
        if crc and crc[0] == 0:
            line += "  CRC mismatch reported"
        if members:
            # a file is intact when every byte of it is in a piece, marked known and equal to the original
            orig = np.frombuffer(original, dtype=np.uint8)
            outa = np.frombuffer(out, dtype=np.uint8)
            maska = np.frombuffer(mask, dtype=np.uint8)
            good = np.zeros(n, dtype=bool)
            for pc, true_at in zip(report["pieces"], offsets):
                o, ln = pc["out_offset"], pc["length"]
                if true_at < 0 or true_at >= n:
                    continue
                ln = min(ln, n - true_at)
                if ln <= 0:
                    continue
                piece, pmask = outa[o:o + ln], maska[o:o + ln]
                good[true_at:true_at + ln] |= (piece == orig[true_at:true_at + ln]) & (pmask != 0)
            intact = sum(1 for _, off, size in members if good[off:off + size].all())
            line += f" {intact:6}/{len(members)}"
        print(line)
        for note in report.get("notes", []):
            print(f"         note: {note}")


main()
