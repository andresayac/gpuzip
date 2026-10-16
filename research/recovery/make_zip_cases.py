"""Damaged ZIP and docx files for the rescue experiments.

    python make_zip_cases.py <out-dir> [MB=48]

lib.zip: Python's standard library, deflated by Python's zipfile, about MB large; copies:
  lib.trunc.zip    cut at 70% (an interrupted download: no central directory)
  lib.nocd.zip     the last 3% zeroed (the central directory destroyed)
  lib.zero4k.zip   4 KB of zeros at 50% (inside some entry's data)
  lib.hdr.zip      a local header in the middle zeroed (its entry's name and sizes lost there)
doc.docx: a document of 300 paragraphs, made here without Word; copies:
  doc.zero.docx    512 zero bytes inside word/document.xml's compressed data
  doc.trunc.docx   cut before the central directory
cases.json: the originals, the paragraphs of the document.
"""
import json
import os
import random
import sys
import zipfile


def build_lib_zip(path, size):
    root = r"C:\Python314\Lib"
    with zipfile.ZipFile(path, "w", zipfile.ZIP_DEFLATED, compresslevel=6) as z:
        total = 0
        for dirpath, dirs, files in os.walk(root):
            dirs.sort()
            for name in sorted(files):
                p = os.path.join(dirpath, name)
                try:
                    st = os.stat(p)
                except OSError:
                    continue
                if st.st_size > (8 << 20):
                    continue
                z.write(p, os.path.relpath(p, root).replace("\\", "/"))
                total += st.st_size
                if total >= size * 4:  # roughly: text compresses about 4:1
                    return


def local_headers(data):
    out, i = [], 0
    while True:
        i = data.find(b"PK\x03\x04", i)
        if i < 0:
            return out
        out.append(i)
        i += 4


def esc(s):
    return s.replace("&", "&amp;").replace("<", "&lt;").replace(">", "&gt;")


def build_docx(path, paragraphs):
    body = "".join(f"<w:p><w:r><w:t xml:space=\"preserve\">{esc(p)}</w:t></w:r></w:p>" for p in paragraphs)
    document = ('<?xml version="1.0" encoding="UTF-8" standalone="yes"?>'
                '<w:document xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main">'
                f'<w:body>{body}<w:sectPr/></w:body></w:document>')
    content_types = ('<?xml version="1.0" encoding="UTF-8" standalone="yes"?>'
                     '<Types xmlns="http://schemas.openxmlformats.org/package/2006/content-types">'
                     '<Default Extension="rels" ContentType="application/vnd.openxmlformats-package.relationships+xml"/>'
                     '<Default Extension="xml" ContentType="application/xml"/>'
                     '<Override PartName="/word/document.xml" ContentType="application/vnd.openxmlformats-officedocument.wordprocessingml.document.main+xml"/>'
                     '</Types>')
    rels = ('<?xml version="1.0" encoding="UTF-8" standalone="yes"?>'
            '<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships">'
            '<Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/officeDocument" Target="word/document.xml"/>'
            '</Relationships>')
    doc_rels = ('<?xml version="1.0" encoding="UTF-8" standalone="yes"?>'
                '<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships"></Relationships>')
    with zipfile.ZipFile(path, "w", zipfile.ZIP_DEFLATED) as z:
        z.writestr("[Content_Types].xml", content_types)
        z.writestr("_rels/.rels", rels)
        z.writestr("word/document.xml", document)
        z.writestr("word/_rels/document.xml.rels", doc_rels)


def main():
    out = sys.argv[1]
    mb = int(sys.argv[2]) if len(sys.argv) > 2 else 48
    os.makedirs(out, exist_ok=True)
    lib = os.path.join(out, "lib.zip")
    build_lib_zip(lib, mb << 20)
    data = open(lib, "rb").read()
    n = len(data)
    open(os.path.join(out, "lib.trunc.zip"), "wb").write(data[: int(n * 0.7)])
    b = bytearray(data)
    b[int(n * 0.97):] = bytes(n - int(n * 0.97))
    open(os.path.join(out, "lib.nocd.zip"), "wb").write(bytes(b))
    b = bytearray(data)
    b[n // 2:n // 2 + 4096] = bytes(4096)
    open(os.path.join(out, "lib.zero4k.zip"), "wb").write(bytes(b))
    hdrs = local_headers(data)
    h = hdrs[len(hdrs) // 3]
    b = bytearray(data)
    b[h:h + 64] = bytes(64)
    open(os.path.join(out, "lib.hdr.zip"), "wb").write(bytes(b))

    rng = random.Random(3)
    words = "the quick brown fox jumps over a lazy dog while reading reports about recovered documents and damaged archives".split()
    paragraphs = [f"Paragraph {i + 1}: " + " ".join(rng.choice(words) for _ in range(rng.randint(8, 40))) + "." for i in range(300)]
    doc = os.path.join(out, "doc.docx")
    build_docx(doc, paragraphs)
    d = open(doc, "rb").read()
    with zipfile.ZipFile(doc) as z:
        info = z.getinfo("word/document.xml")
        start = info.header_offset + 30 + len(info.filename.encode()) + len(info.extra)
        csize = info.compress_size
    b = bytearray(d)
    p = start + csize // 2
    b[p:p + 512] = bytes(512)
    open(os.path.join(out, "doc.zero.docx"), "wb").write(bytes(b))
    with zipfile.ZipFile(doc) as z:
        last = max(i.header_offset + 30 + len(i.filename.encode()) + len(i.extra) + i.compress_size for i in z.infolist())
    open(os.path.join(out, "doc.trunc.docx"), "wb").write(d[:last])
    json.dump({"lib": lib, "lib_size": n, "docx": doc, "paragraphs": paragraphs, "zeroed_header": h},
              open(os.path.join(out, "cases.json"), "w"), indent=1)
    print(f"lib.zip {n >> 20} MB with {len(hdrs)} entries; doc.docx {len(d)} bytes; 6 damaged copies in {out}")


main()
