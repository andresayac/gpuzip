"""Checks what `gpuzip rescue` extracted from a damaged .tar.gz against the original tar.

    python check_extract.py <original.tar> <extracted-dir> <rescue.json>

Every file the report calls intact must be identical to the original (no silent errors);
counts the identical, different and missing files overall.
"""
import hashlib
import json
import os
import sys
import tarfile


def main():
    orig, out, rep_path = sys.argv[1:4]
    raw = open(rep_path, "rb").read()  # PowerShell's > writes UTF-16 or a BOM
    text = raw.decode("utf-16") if raw[:2] in (b"\xff\xfe", b"\xfe\xff") else raw.decode("utf-8-sig")
    rep = json.loads(text.strip().splitlines()[-1])
    tar_info = rep["tar"]
    suspect, partial = set(tar_info["suspect"]), set(tar_info["partial"])
    identical = different = missing = 0
    intact_wrong = []
    with tarfile.open(orig) as t:
        for m in t:
            if not m.isfile():
                continue
            p = os.path.join(out, m.name.replace("/", os.sep))
            if not os.path.exists(p):
                missing += 1
                continue
            want = hashlib.sha256(t.extractfile(m).read()).hexdigest()
            got = hashlib.sha256(open(p, "rb").read()).hexdigest()
            if want == got:
                identical += 1
            else:
                different += 1
                if m.name not in suspect and m.name not in partial:
                    intact_wrong.append(m.name)
    print(f"identical {identical}, different {different}, missing {missing}; reported intact {tar_info['intact']}, "
          f"possibly damaged {len(suspect)}, partial {len(partial)}, unreadable regions {len(tar_info['skipped'])}")
    if intact_wrong:
        print(f"FAIL: {len(intact_wrong)} files called intact differ from the original, e.g. {intact_wrong[:3]}")
        sys.exit(1)
    print("every file called intact is identical to the original")


main()
