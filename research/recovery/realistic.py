"""Damaged files the way real ones get damaged, for testing `gpuzip rescue`.

    python realistic.py <out-dir>

Originals (made here, from data already on this machine, nothing personal):
  logs.txt.gz     32 MB of a Windows CBS log, gzip -6
  code.tar.gz     32 MB tar of Python's standard library, gzip -6
  lib.zip         Python's standard library, Python zipfile -6 (about 17 MB)
  report.docx     a 2,000-paragraph Word document with styles, properties and two images
  budget.xlsx     a 6-sheet workbook written by openpyxl (strings, numbers, formulas)
  slides.pptx     a 12-slide presentation with images

Failures, each modelled on how files really break:
  ftp_ascii      sent through FTP in ASCII mode, Unix to Windows: every LF became CR LF
  ascii_strip    the reverse, Windows to Unix: every CR LF became LF
  bad_sectors    read off a failing disk with ddrescue: 3 aligned 4 KB sectors are zeros
  flash_ff       a USB stick or SD card lost an erase block: 128 KB aligned of 0xFF
  crosslinked    chkdsk cross-linked two 4 KB clusters: they hold another file's data
  torrent_holes  a download that stalled at 97%: pieces never received are zeros
  cut            an interrupted download: the last 17% missing
  resume_dup     a download resumed from the wrong offset: 64 KB written twice
  resume_gap     a download resumed past where it stopped: 512 KB missing in the middle
  crash_tail     the computer crashed while copying: the last 20% are zeros (preallocated)
  bitrot         bits flipped by failing RAM or storage: 8 random bit flips

Writes <name>.<failure>.<ext> for every pair, and manifest.json with the originals, the text
units of each Office document (paragraphs, cells, slide texts) and what each failure did.
"""
import io
import json
import os
import random
import sys
import tarfile
import zipfile
import zlib

HERE = os.path.dirname(os.path.abspath(__file__))
SECTOR = 4096


# ---------------------------------------------------------------- originals

def make_log_gz(out, size=32 << 20):
    win = os.environ.get("SystemRoot", r"C:\Windows")
    d = os.path.join(win, "Logs", "CBS")
    src = max((os.path.join(d, n) for n in os.listdir(d) if n.endswith(".log")), key=os.path.getsize)
    data = open(src, "rb").read(size)
    path = os.path.join(out, "logs.txt.gz")
    c = zlib.compressobj(6, zlib.DEFLATED, 31)
    open(path, "wb").write(c.compress(data) + c.flush())
    open(os.path.join(out, "logs.txt"), "wb").write(data)
    return path


def make_code_tgz(out, size=32 << 20):
    root = r"C:\Python314\Lib"
    tar_path = os.path.join(out, "code.tar")
    with tarfile.open(tar_path, "w", format=tarfile.PAX_FORMAT) as t:
        total = 0
        for dirpath, dirs, files in os.walk(root):
            dirs.sort()
            for name in sorted(files):
                p = os.path.join(dirpath, name)
                try:
                    s = os.path.getsize(p)
                except OSError:
                    continue
                if s > (4 << 20):
                    continue
                t.add(p, os.path.relpath(p, root).replace("\\", "/"))
                total += s
                if total >= size:
                    break
            if total >= size:
                break
    data = open(tar_path, "rb").read()
    path = os.path.join(out, "code.tar.gz")
    c = zlib.compressobj(6, zlib.DEFLATED, 31)
    open(path, "wb").write(c.compress(data) + c.flush())
    return path


def make_lib_zip(out, size=17 << 20):
    root = r"C:\Python314\Lib"
    path = os.path.join(out, "lib.zip")
    with zipfile.ZipFile(path, "w", zipfile.ZIP_DEFLATED, compresslevel=6) as z:
        total = 0
        for dirpath, dirs, files in os.walk(root):
            dirs.sort()
            for name in sorted(files):
                p = os.path.join(dirpath, name)
                try:
                    s = os.path.getsize(p)
                except OSError:
                    continue
                if s > (8 << 20):
                    continue
                z.write(p, os.path.relpath(p, root).replace("\\", "/"))
                total += s
                if total >= size * 4:  # source compresses about 4:1
                    return path
    return path


def image_bytes(kind, seed, w=640, h=400):
    from PIL import Image, ImageDraw
    rng = random.Random(seed)
    img = Image.new("RGB", (w, h), (250, 250, 250))
    d = ImageDraw.Draw(img)
    for i in range(12):  # a bar chart, roughly what documents hold
        x = 40 + i * 48
        bh = rng.randint(40, h - 80)
        d.rectangle([x, h - 40 - bh, x + 30, h - 40], fill=(40 + i * 15, 90, 200 - i * 10))
    for y in range(40, h - 40, 40):
        d.line([30, y, w - 20, y], fill=(220, 220, 220))
    buf = io.BytesIO()
    img.save(buf, format=kind)
    return buf.getvalue()


def esc(s):
    return s.replace("&", "&amp;").replace("<", "&lt;").replace(">", "&gt;")


WORDS = ("quarterly revenue forecast budget customer delivery project timeline review approval team "
         "meeting summary analysis contract supplier invoice payment schedule risk mitigation report "
         "update milestone resource allocation strategy market growth target performance").split()


def sentence(rng, lo=8, hi=30):
    return " ".join(rng.choice(WORDS) for _ in range(rng.randint(lo, hi)))


DOCX_CT = ('<?xml version="1.0" encoding="UTF-8" standalone="yes"?>\n'
           '<Types xmlns="http://schemas.openxmlformats.org/package/2006/content-types">'
           '<Default Extension="rels" ContentType="application/vnd.openxmlformats-package.relationships+xml"/>'
           '<Default Extension="xml" ContentType="application/xml"/>'
           '<Default Extension="png" ContentType="image/png"/><Default Extension="jpeg" ContentType="image/jpeg"/>'
           '<Override PartName="/word/document.xml" ContentType="application/vnd.openxmlformats-officedocument.wordprocessingml.document.main+xml"/>'
           '<Override PartName="/word/styles.xml" ContentType="application/vnd.openxmlformats-officedocument.wordprocessingml.styles+xml"/>'
           '<Override PartName="/word/settings.xml" ContentType="application/vnd.openxmlformats-officedocument.wordprocessingml.settings+xml"/>'
           '<Override PartName="/docProps/core.xml" ContentType="application/vnd.openxmlformats-package.core-properties+xml"/>'
           '<Override PartName="/docProps/app.xml" ContentType="application/vnd.openxmlformats-officedocument.extended-properties+xml"/>'
           '</Types>')
RELS = ('<?xml version="1.0" encoding="UTF-8" standalone="yes"?>\n'
        '<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships">'
        '<Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/officeDocument" Target="{main}"/>'
        '<Relationship Id="rId2" Type="http://schemas.openxmlformats.org/package/2006/relationships/metadata/core-properties" Target="docProps/core.xml"/>'
        '<Relationship Id="rId3" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/extended-properties" Target="docProps/app.xml"/>'
        '</Relationships>')
CORE = ('<?xml version="1.0" encoding="UTF-8" standalone="yes"?>\n'
        '<cp:coreProperties xmlns:cp="http://schemas.openxmlformats.org/package/2006/metadata/core-properties" '
        'xmlns:dc="http://purl.org/dc/elements/1.1/" xmlns:dcterms="http://purl.org/dc/terms/" '
        'xmlns:xsi="http://www.w3.org/2001/XMLSchema-instance"><dc:title>{title}</dc:title><dc:creator>gpuzip tests</dc:creator>'
        '<dcterms:created xsi:type="dcterms:W3CDTF">2026-10-17T09:00:00Z</dcterms:created></cp:coreProperties>')
APP = ('<?xml version="1.0" encoding="UTF-8" standalone="yes"?>\n'
       '<Properties xmlns="http://schemas.openxmlformats.org/officeDocument/2006/extended-properties"><Application>gpuzip tests</Application></Properties>')


def make_docx(out):
    rng = random.Random(11)
    units = []
    body = []
    for s in range(20):
        heading = f"Section {s + 1}: {sentence(rng, 3, 6)}"
        units.append(heading)
        body.append(f'<w:p><w:pPr><w:pStyle w:val="Heading1"/></w:pPr><w:r><w:t>{esc(heading)}</w:t></w:r></w:p>')
        for p in range(100):
            text = f"Paragraph {s + 1}.{p + 1}: {sentence(rng)}."
            units.append(text)
            bold = rng.random() < 0.2
            body.append('<w:p><w:pPr><w:spacing w:after="120"/></w:pPr><w:r>' + ('<w:rPr><w:b/></w:rPr>' if bold else '')
                        + f'<w:t xml:space="preserve">{esc(text)}</w:t></w:r></w:p>')
    document = ('<?xml version="1.0" encoding="UTF-8" standalone="yes"?>\n'
                '<w:document xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main" '
                'xmlns:r="http://schemas.openxmlformats.org/officeDocument/2006/relationships">'
                f'<w:body>{"".join(body)}<w:sectPr><w:pgSz w:w="11906" w:h="16838"/></w:sectPr></w:body></w:document>')
    styles = ('<?xml version="1.0" encoding="UTF-8" standalone="yes"?>\n'
              '<w:styles xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main">'
              '<w:style w:type="paragraph" w:default="1" w:styleId="Normal"><w:name w:val="Normal"/></w:style>'
              '<w:style w:type="paragraph" w:styleId="Heading1"><w:name w:val="heading 1"/><w:rPr><w:b/><w:sz w:val="32"/></w:rPr></w:style>'
              '</w:styles>')
    settings = ('<?xml version="1.0" encoding="UTF-8" standalone="yes"?>\n'
                '<w:settings xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main"><w:defaultTabStop w:val="708"/></w:settings>')
    doc_rels = ('<?xml version="1.0" encoding="UTF-8" standalone="yes"?>\n'
                '<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships">'
                '<Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/styles" Target="styles.xml"/>'
                '<Relationship Id="rId2" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/settings" Target="settings.xml"/>'
                '<Relationship Id="rId3" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/image" Target="media/image1.png"/>'
                '<Relationship Id="rId4" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/image" Target="media/image2.jpeg"/>'
                '</Relationships>')
    path = os.path.join(out, "report.docx")
    with zipfile.ZipFile(path, "w", zipfile.ZIP_DEFLATED) as z:  # Word deflates every part, images too
        z.writestr("[Content_Types].xml", DOCX_CT)
        z.writestr("_rels/.rels", RELS.replace("{main}", "word/document.xml"))
        z.writestr("word/document.xml", document)
        z.writestr("word/_rels/document.xml.rels", doc_rels)
        z.writestr("word/styles.xml", styles)
        z.writestr("word/settings.xml", settings)
        z.writestr("word/media/image1.png", image_bytes("PNG", 1))
        z.writestr("word/media/image2.jpeg", image_bytes("JPEG", 2))
        z.writestr("docProps/core.xml", CORE.replace("{title}", "Quarterly report"))
        z.writestr("docProps/app.xml", APP)
    return path, units


def make_xlsx(out):
    from openpyxl import Workbook
    rng = random.Random(12)
    wb = Workbook()
    units = []
    for s in range(6):
        ws = wb.active if s == 0 else wb.create_sheet()
        ws.title = f"Region {s + 1}"
        ws.append(["Item", "Description", "Quantity", "Unit price", "Total"])
        for r in range(400):
            item = f"Item {s + 1}-{r + 1:04d}"
            desc = sentence(rng, 3, 8)
            qty = rng.randint(1, 500)
            price = round(rng.uniform(1, 999), 2)
            ws.append([item, desc, qty, price, f"=C{r + 2}*D{r + 2}"])
            units.append(item)
    path = os.path.join(out, "budget.xlsx")
    wb.save(path)
    return path, units


def make_pptx(out):
    rng = random.Random(13)
    units = []
    slides = []
    for s in range(12):
        title = f"Slide {s + 1}: {sentence(rng, 2, 5)}"
        points = [f"Point {s + 1}.{k + 1}: {sentence(rng, 5, 14)}" for k in range(8)]
        units += [title] + points
        paras = "".join(f"<a:p><a:r><a:t>{esc(p)}</a:t></a:r></a:p>" for p in points)
        slides.append('<?xml version="1.0" encoding="UTF-8" standalone="yes"?>\n'
                      '<p:sld xmlns:a="http://schemas.openxmlformats.org/drawingml/2006/main" '
                      'xmlns:p="http://schemas.openxmlformats.org/presentationml/2006/main" '
                      'xmlns:r="http://schemas.openxmlformats.org/officeDocument/2006/relationships"><p:cSld><p:spTree>'
                      '<p:nvGrpSpPr><p:cNvPr id="1" name=""/><p:cNvGrpSpPr/><p:nvPr/></p:nvGrpSpPr><p:grpSpPr/>'
                      f'<p:sp><p:nvSpPr><p:cNvPr id="2" name="Title"/><p:cNvSpPr/><p:nvPr/></p:nvSpPr><p:spPr/><p:txBody><a:bodyPr/><a:p><a:r><a:t>{esc(title)}</a:t></a:r></a:p></p:txBody></p:sp>'
                      f'<p:sp><p:nvSpPr><p:cNvPr id="3" name="Body"/><p:cNvSpPr/><p:nvPr/></p:nvSpPr><p:spPr/><p:txBody><a:bodyPr/>{paras}</p:txBody></p:sp>'
                      '</p:spTree></p:cSld></p:sld>')
    ct = ('<?xml version="1.0" encoding="UTF-8" standalone="yes"?>\n'
          '<Types xmlns="http://schemas.openxmlformats.org/package/2006/content-types">'
          '<Default Extension="rels" ContentType="application/vnd.openxmlformats-package.relationships+xml"/>'
          '<Default Extension="xml" ContentType="application/xml"/><Default Extension="png" ContentType="image/png"/>'
          '<Override PartName="/ppt/presentation.xml" ContentType="application/vnd.openxmlformats-officedocument.presentationml.presentation.main+xml"/>'
          + "".join(f'<Override PartName="/ppt/slides/slide{i + 1}.xml" ContentType="application/vnd.openxmlformats-officedocument.presentationml.slide+xml"/>' for i in range(12))
          + '</Types>')
    pres = ('<?xml version="1.0" encoding="UTF-8" standalone="yes"?>\n'
            '<p:presentation xmlns:a="http://schemas.openxmlformats.org/drawingml/2006/main" '
            'xmlns:p="http://schemas.openxmlformats.org/presentationml/2006/main" '
            'xmlns:r="http://schemas.openxmlformats.org/officeDocument/2006/relationships"><p:sldIdLst>'
            + "".join(f'<p:sldId id="{256 + i}" r:id="rId{i + 1}"/>' for i in range(12))
            + '</p:sldIdLst><p:sldSz cx="12192000" cy="6858000"/></p:presentation>')
    pres_rels = ('<?xml version="1.0" encoding="UTF-8" standalone="yes"?>\n'
                 '<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships">'
                 + "".join(f'<Relationship Id="rId{i + 1}" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/slide" Target="slides/slide{i + 1}.xml"/>' for i in range(12))
                 + '</Relationships>')
    path = os.path.join(out, "slides.pptx")
    with zipfile.ZipFile(path, "w", zipfile.ZIP_DEFLATED) as z:
        z.writestr("[Content_Types].xml", ct)
        z.writestr("_rels/.rels", RELS.replace("{main}", "ppt/presentation.xml"))
        z.writestr("ppt/presentation.xml", pres)
        z.writestr("ppt/_rels/presentation.xml.rels", pres_rels)
        for i, x in enumerate(slides):
            z.writestr(f"ppt/slides/slide{i + 1}.xml", x)
            if i % 3 == 0:
                z.writestr(f"ppt/media/image{i + 1}.png", image_bytes("PNG", 100 + i, 960, 540))
        z.writestr("docProps/core.xml", CORE.replace("{title}", "Planning"))
        z.writestr("docProps/app.xml", APP)
    return path, units


# ---------------------------------------------------------------- failures

def ftp_ascii(b, rng):
    return b.replace(b"\n", b"\r\n"), {"lf_to_crlf": b.count(b"\n")}


def ascii_strip(b, rng):
    return b.replace(b"\r\n", b"\n"), {"crlf_to_lf": b.count(b"\r\n")}


def bad_sectors(b, rng):
    b = bytearray(b)
    n = len(b) // SECTOR
    hit = sorted(rng.sample(range(1, max(2, n - 1)), min(3, max(1, n - 2))))
    for s in hit:
        b[s * SECTOR:(s + 1) * SECTOR] = bytes(SECTOR)
    return bytes(b), {"zeroed_sectors": [s * SECTOR for s in hit]}


def flash_ff(b, rng):
    blk = 128 << 10 if len(b) > (1 << 20) else 16 << 10  # small files: a smaller page
    b = bytearray(b)
    n = len(b) // blk
    k = rng.randrange(0, max(1, n)) if n else 0
    b[k * blk:(k + 1) * blk] = b"\xff" * len(b[k * blk:(k + 1) * blk])
    return bytes(b), {"ff_block": [k * blk, blk]}


def crosslinked(b, rng, other=None):
    b = bytearray(b)
    n = len(b) // SECTOR
    hit = sorted(rng.sample(range(1, max(2, n - 1)), min(2, max(1, n - 2))))
    for s in hit:
        o = rng.randrange(0, len(other) - SECTOR)
        b[s * SECTOR:(s + 1) * SECTOR] = other[o:o + SECTOR]
    return bytes(b), {"crosslinked_clusters": [s * SECTOR for s in hit]}


def torrent_holes(b, rng):
    piece = 256 << 10 if len(b) > (8 << 20) else 16 << 10
    b = bytearray(b)
    n = len(b) // piece
    k = max(1, int(n * 0.03))
    hit = sorted(rng.sample(range(n), min(k, n))) if n else []
    for s in hit:
        b[s * piece:(s + 1) * piece] = bytes(piece)
    return bytes(b), {"missing_pieces": [s * piece for s in hit], "piece": piece}


def cut(b, rng):
    return b[: int(len(b) * 0.83)], {"cut_at": int(len(b) * 0.83)}


def resume_dup(b, rng):
    dup = min(64 << 10, len(b) // 8)
    p = int(len(b) * 0.45)
    return b[:p + dup] + b[p:], {"duplicated": [p, dup]}


def resume_gap(b, rng):
    gap = min(512 << 10, len(b) // 8)
    p = int(len(b) * 0.55)
    return b[:p] + b[p + gap:], {"missing": [p, gap]}


def crash_tail(b, rng):
    p = int(len(b) * 0.8)
    return b[:p] + bytes(len(b) - p), {"zeros_from": p}


def bitrot(b, rng):
    b = bytearray(b)
    flips = sorted((rng.randrange(len(b)), rng.randrange(8)) for _ in range(8))
    for pos, bit in flips:
        b[pos] ^= 1 << bit
    return bytes(b), {"flipped": flips}


FAILURES = {f.__name__: f for f in (ftp_ascii, ascii_strip, bad_sectors, flash_ff, crosslinked, torrent_holes, cut,
                                     resume_dup, resume_gap, crash_tail, bitrot)}


def main():
    out = sys.argv[1]
    os.makedirs(out, exist_ok=True)
    print("making the originals...", flush=True)
    originals = {"logs.txt.gz": {"kind": "gz", "path": make_log_gz(out), "plain": os.path.join(out, "logs.txt")},
                 "code.tar.gz": {"kind": "tgz", "path": make_code_tgz(out), "tar": os.path.join(out, "code.tar")},
                 "lib.zip": {"kind": "zip", "path": make_lib_zip(out)}}
    for name, maker in (("report.docx", make_docx), ("budget.xlsx", make_xlsx), ("slides.pptx", make_pptx)):
        path, units = maker(out)
        originals[name] = {"kind": "office", "path": path, "units": units}
    other = open(originals["lib.zip"]["path"], "rb").read()  # the "other file" of cross-linked clusters
    cases = []
    for name, o in originals.items():
        data = open(o["path"], "rb").read()
        o["size"] = len(data)
        stem, ext = name.rsplit(".", 1) if not name.endswith(".tar.gz") else ("code", "tar.gz")
        if name == "logs.txt.gz":
            stem, ext = "logs", "txt.gz"
        for k, (fname, f) in enumerate(FAILURES.items()):
            rng = random.Random(1000 + k * 17 + len(name))
            damaged, what = f(data, rng, other) if fname == "crosslinked" else f(data, rng)
            p = os.path.join(out, f"{stem}.{fname}.{ext}")
            open(p, "wb").write(damaged)
            cases.append({"original": name, "failure": fname, "path": p, "what": what})
        print(f"  {name}: {len(data):,} bytes, {len(FAILURES)} damaged copies", flush=True)
    json.dump({"originals": originals, "cases": cases}, open(os.path.join(out, "manifest.json"), "w"), indent=1)
    print(f"{len(cases)} damaged files in {out}")


if __name__ == "__main__":
    main()
