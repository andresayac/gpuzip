"""Checks what `gpuzip rescue` made of a damaged ZIP or docx against the original.

    python check_zip.py <original.zip> <extracted-dir> <report.json>

Every entry the report calls intact must be identical to the original; prints how many entries
came back identical, different, missing, by status. For a docx: the recovered text's share of
the original paragraphs, whether the repaired document.xml is well-formed, and whether the
rebuilt document opens as a ZIP whose document.xml parses.
"""
import json
import os
import sys
import xml.etree.ElementTree as ET
import zipfile


def load_report(path):
    raw = open(path, "rb").read()
    text = raw.decode("utf-16") if raw[:2] in (b"\xff\xfe", b"\xfe\xff") else raw.decode("utf-8-sig")
    return json.loads(text.strip().splitlines()[-1])


def main():
    orig, out, rep_path = sys.argv[1:4]
    rep = load_report(rep_path)
    status = {e["name"]: e["status"] for e in rep["entries"]}
    identical = different = missing = 0
    intact_wrong = []
    by_status = {}
    with zipfile.ZipFile(orig) as z:
        for info in z.infolist():
            if info.is_dir():
                continue
            p = os.path.join(out, info.filename.replace("/", os.sep))
            st = status.get(info.filename, "absent")
            by_status.setdefault(st, [0, 0])
            if not os.path.exists(p):
                missing += 1
                continue
            same = open(p, "rb").read() == z.read(info)
            by_status[st][0 if same else 1] += 1
            if same:
                identical += 1
            else:
                different += 1
                if st == "intact":
                    intact_wrong.append(info.filename)
    print(f"identical {identical}, different {different}, missing {missing}; by status (identical, different): "
          + ", ".join(f"{k} {v}" for k, v in sorted(by_status.items())))
    print(f"directory ok: {rep['directory_ok']}, scanned headers: {rep['scanned_headers']}, from scan: {rep['from_scan']}, exit code expected 2: {not rep['clean']}")
    ok = not intact_wrong
    if intact_wrong:
        print(f"FAIL: {len(intact_wrong)} entries called intact differ, e.g. {intact_wrong[:3]}")
    if rep.get("office"):
        of = rep["office"]
        cases = json.load(open(os.path.join(os.path.dirname(orig), "cases.json")))
        text = open(of["text_file"], encoding="utf-8").read() if of.get("text_file") else ""
        found = sum(1 for p in cases["paragraphs"] if p in text)
        print(f"docx: {found} of {len(cases['paragraphs'])} paragraphs in the recovered text; repaired parts {of['repaired_parts']}")
        doc_xml = os.path.join(out, "word", "document.xml")
        try:
            ET.parse(doc_xml)
            print("docx: the (repaired) word/document.xml is well-formed")
        except ET.ParseError as e:
            print(f"FAIL: word/document.xml is not well-formed: {e}")
            ok = False
        if of.get("rebuilt"):
            try:
                with zipfile.ZipFile(of["rebuilt"]) as z:
                    ET.fromstring(z.read("word/document.xml"))
                    print(f"docx: rebuilt document {os.path.basename(of['rebuilt'])} opens, {len(z.namelist())} parts, document.xml parses")
            except Exception as e:  # noqa: BLE001
                print(f"FAIL: the rebuilt document does not open: {e}")
                ok = False
    print("OK" if ok else "FAILED")
    sys.exit(0 if ok else 1)


main()
