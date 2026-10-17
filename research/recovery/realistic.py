"""Damaged files the way real ones get damaged, for testing `gpuzip rescue`.

    python realistic.py <out-dir> [--only slides.pptx]   (--only: remake just those Office files)

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


PML = ('xmlns:a="http://schemas.openxmlformats.org/drawingml/2006/main" '
       'xmlns:p="http://schemas.openxmlformats.org/presentationml/2006/main" '
       'xmlns:r="http://schemas.openxmlformats.org/officeDocument/2006/relationships"')
REL = "http://schemas.openxmlformats.org/officeDocument/2006/relationships"
XML_HEAD = '<?xml version="1.0" encoding="UTF-8" standalone="yes"?>\n'
GROUP = '<p:nvGrpSpPr><p:cNvPr id="1" name=""/><p:cNvGrpSpPr/><p:nvPr/></p:nvGrpSpPr><p:grpSpPr/>'
# a theme with what PowerPoint requires of one: colours, fonts, and three of each format style
THEME = (XML_HEAD + '<a:theme xmlns:a="http://schemas.openxmlformats.org/drawingml/2006/main" name="Office"><a:themeElements>'
         '<a:clrScheme name="Office"><a:dk1><a:sysClr val="windowText" lastClr="000000"/></a:dk1>'
         '<a:lt1><a:sysClr val="window" lastClr="FFFFFF"/></a:lt1><a:dk2><a:srgbClr val="44546A"/></a:dk2>'
         '<a:lt2><a:srgbClr val="E7E6E6"/></a:lt2>'
         + "".join(f'<a:accent{i + 1}><a:srgbClr val="{c}"/></a:accent{i + 1}>'
                   for i, c in enumerate(("4472C4", "ED7D31", "A5A5A5", "FFC000", "5B9BD5", "70AD47")))
         + '<a:hlink><a:srgbClr val="0563C1"/></a:hlink><a:folHlink><a:srgbClr val="954F72"/></a:folHlink></a:clrScheme>'
         '<a:fontScheme name="Office"><a:majorFont><a:latin typeface="Calibri Light"/><a:ea typeface=""/><a:cs typeface=""/></a:majorFont>'
         '<a:minorFont><a:latin typeface="Calibri"/><a:ea typeface=""/><a:cs typeface=""/></a:minorFont></a:fontScheme>'
         '<a:fmtScheme name="Office"><a:fillStyleLst>' + '<a:solidFill><a:schemeClr val="phClr"/></a:solidFill>' * 3 + '</a:fillStyleLst>'
         '<a:lnStyleLst>' + '<a:ln w="6350"><a:solidFill><a:schemeClr val="phClr"/></a:solidFill></a:ln>' * 3 + '</a:lnStyleLst>'
         '<a:effectStyleLst>' + '<a:effectStyle><a:effectLst/></a:effectStyle>' * 3 + '</a:effectStyleLst>'
         '<a:bgFillStyleLst>' + '<a:solidFill><a:schemeClr val="phClr"/></a:solidFill>' * 3 + '</a:bgFillStyleLst>'
         '</a:fmtScheme></a:themeElements><a:objectDefaults/><a:extraClrSchemeLst/></a:theme>')


def rels_xml(*rels):
    return (XML_HEAD + '<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships">'
            + "".join(f'<Relationship Id="{i}" Type="{REL}/{t}" Target="{target}"/>' for i, t, target in rels) + '</Relationships>')


def text_box(sid, name, x, y, cx, cy, paras):
    return (f'<p:sp><p:nvSpPr><p:cNvPr id="{sid}" name="{name}"/><p:cNvSpPr txBox="1"/><p:nvPr/></p:nvSpPr>'
            f'<p:spPr><a:xfrm><a:off x="{x}" y="{y}"/><a:ext cx="{cx}" cy="{cy}"/></a:xfrm><a:prstGeom prst="rect"><a:avLst/></a:prstGeom></p:spPr>'
            f'<p:txBody><a:bodyPr wrap="square"/><a:lstStyle/>{paras}</p:txBody></p:sp>')


def make_pptx(out):
    """A presentation as PowerPoint writes one: master, layout and theme, every slide on the
    layout, and the images placed on the slides that hold them"""
    rng = random.Random(13)
    units = []
    slides = []
    for s in range(12):
        title = f"Slide {s + 1}: {sentence(rng, 2, 5)}"
        points = [f"Point {s + 1}.{k + 1}: {sentence(rng, 5, 14)}" for k in range(8)]
        units += [title] + points
        has_image = s % 3 == 0
        body_w = 6858000 if has_image else 11277600
        paras = "".join(f'<a:p><a:r><a:rPr lang="en-US" sz="1400"/><a:t>{esc(p)}</a:t></a:r></a:p>' for p in points)
        pic = ('<p:pic><p:nvPicPr><p:cNvPr id="4" name="Chart"/><p:cNvPicPr><a:picLocks noChangeAspect="1"/></p:cNvPicPr><p:nvPr/></p:nvPicPr>'
               '<p:blipFill><a:blip r:embed="rId2"/><a:stretch><a:fillRect/></a:stretch></p:blipFill>'
               '<p:spPr><a:xfrm><a:off x="7543800" y="1600200"/><a:ext cx="4191000" cy="2357438"/></a:xfrm><a:prstGeom prst="rect"><a:avLst/></a:prstGeom></p:spPr></p:pic>'
               if has_image else '')
        slides.append(XML_HEAD + f'<p:sld {PML}><p:cSld><p:spTree>{GROUP}'
                      + text_box(2, "Title", 457200, 274638, 11277600, 1143000,
                                 f'<a:p><a:r><a:rPr lang="en-US" sz="3200" b="1"/><a:t>{esc(title)}</a:t></a:r></a:p>')
                      + text_box(3, "Body", 457200, 1600200, body_w, 4525963, paras)
                      + pic + '</p:spTree></p:cSld><p:clrMapOvr><a:masterClrMapping/></p:clrMapOvr></p:sld>')
    pml_ct = "application/vnd.openxmlformats-officedocument.presentationml"
    ct = (XML_HEAD + '<Types xmlns="http://schemas.openxmlformats.org/package/2006/content-types">'
          '<Default Extension="rels" ContentType="application/vnd.openxmlformats-package.relationships+xml"/>'
          '<Default Extension="xml" ContentType="application/xml"/><Default Extension="png" ContentType="image/png"/>'
          f'<Override PartName="/ppt/presentation.xml" ContentType="{pml_ct}.presentation.main+xml"/>'
          f'<Override PartName="/ppt/slideMasters/slideMaster1.xml" ContentType="{pml_ct}.slideMaster+xml"/>'
          f'<Override PartName="/ppt/slideLayouts/slideLayout1.xml" ContentType="{pml_ct}.slideLayout+xml"/>'
          '<Override PartName="/ppt/theme/theme1.xml" ContentType="application/vnd.openxmlformats-officedocument.theme+xml"/>'
          + "".join(f'<Override PartName="/ppt/slides/slide{i + 1}.xml" ContentType="{pml_ct}.slide+xml"/>' for i in range(12))
          + '<Override PartName="/docProps/core.xml" ContentType="application/vnd.openxmlformats-package.core-properties+xml"/>'
          '<Override PartName="/docProps/app.xml" ContentType="application/vnd.openxmlformats-officedocument.extended-properties+xml"/>'
          '</Types>')
    pres = (XML_HEAD + f'<p:presentation {PML} saveSubsetFonts="1">'
            '<p:sldMasterIdLst><p:sldMasterId id="2147483648" r:id="rId13"/></p:sldMasterIdLst><p:sldIdLst>'
            + "".join(f'<p:sldId id="{256 + i}" r:id="rId{i + 1}"/>' for i in range(12))
            + '</p:sldIdLst><p:sldSz cx="12192000" cy="6858000"/><p:notesSz cx="6858000" cy="9144000"/></p:presentation>')
    pres_rels = rels_xml(*[(f"rId{i + 1}", "slide", f"slides/slide{i + 1}.xml") for i in range(12)],
                         ("rId13", "slideMaster", "slideMasters/slideMaster1.xml"), ("rId14", "theme", "theme/theme1.xml"))
    master = (XML_HEAD + f'<p:sldMaster {PML}><p:cSld><p:bg><p:bgRef idx="1001"><a:schemeClr val="bg1"/></p:bgRef></p:bg>'
              f'<p:spTree>{GROUP}</p:spTree></p:cSld>'
              '<p:clrMap bg1="lt1" tx1="dk1" bg2="lt2" tx2="dk2" accent1="accent1" accent2="accent2" accent3="accent3" '
              'accent4="accent4" accent5="accent5" accent6="accent6" hlink="hlink" folHlink="folHlink"/>'
              '<p:sldLayoutIdLst><p:sldLayoutId id="2147483649" r:id="rId1"/></p:sldLayoutIdLst></p:sldMaster>')
    layout = (XML_HEAD + f'<p:sldLayout {PML} type="blank" preserve="1"><p:cSld name="Blank"><p:spTree>{GROUP}</p:spTree></p:cSld>'
              '<p:clrMapOvr><a:masterClrMapping/></p:clrMapOvr></p:sldLayout>')
    path = os.path.join(out, "slides.pptx")
    with zipfile.ZipFile(path, "w", zipfile.ZIP_DEFLATED) as z:
        z.writestr("[Content_Types].xml", ct)
        z.writestr("_rels/.rels", RELS.replace("{main}", "ppt/presentation.xml"))
        z.writestr("ppt/presentation.xml", pres)
        z.writestr("ppt/_rels/presentation.xml.rels", pres_rels)
        for i, x in enumerate(slides):
            z.writestr(f"ppt/slides/slide{i + 1}.xml", x)
            rels = [("rId1", "slideLayout", "../slideLayouts/slideLayout1.xml")]
            if i % 3 == 0:
                rels.append(("rId2", "image", f"../media/image{i + 1}.png"))
                z.writestr(f"ppt/media/image{i + 1}.png", image_bytes("PNG", 100 + i, 960, 540))
            z.writestr(f"ppt/slides/_rels/slide{i + 1}.xml.rels", rels_xml(*rels))
        z.writestr("ppt/slideMasters/slideMaster1.xml", master)
        z.writestr("ppt/slideMasters/_rels/slideMaster1.xml.rels",
                   rels_xml(("rId1", "slideLayout", "../slideLayouts/slideLayout1.xml"), ("rId2", "theme", "../theme/theme1.xml")))
        z.writestr("ppt/slideLayouts/slideLayout1.xml", layout)
        z.writestr("ppt/slideLayouts/_rels/slideLayout1.xml.rels", rels_xml(("rId1", "slideMaster", "../slideMasters/slideMaster1.xml")))
        z.writestr("ppt/theme/theme1.xml", THEME)
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
    # --only report.docx,slides.pptx: remake those Office originals and their damaged copies, keep
    # the rest of an earlier run (the log and the library it was made from change over time)
    only = set(sys.argv[sys.argv.index("--only") + 1].split(",")) if "--only" in sys.argv else None
    os.makedirs(out, exist_ok=True)
    makers = (("report.docx", make_docx), ("budget.xlsx", make_xlsx), ("slides.pptx", make_pptx))
    if only:
        earlier = json.load(open(os.path.join(out, "manifest.json")))
        originals = earlier["originals"]
        kept = [c for c in earlier["cases"] if c["original"] not in only]
        makers = tuple((n, m) for n, m in makers if n in only)
    else:
        print("making the originals...", flush=True)
        originals = {"logs.txt.gz": {"kind": "gz", "path": make_log_gz(out), "plain": os.path.join(out, "logs.txt")},
                     "code.tar.gz": {"kind": "tgz", "path": make_code_tgz(out), "tar": os.path.join(out, "code.tar")},
                     "lib.zip": {"kind": "zip", "path": make_lib_zip(out)}}
        kept = []
    for name, maker in makers:
        path, units = maker(out)
        originals[name] = {"kind": "office", "path": path, "units": units}
    other = open(originals["lib.zip"]["path"], "rb").read()  # the "other file" of cross-linked clusters
    cases = []
    for name, o in originals.items():
        if only and name not in only:
            continue
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
    order = list(originals)
    cases = sorted(kept + cases, key=lambda c: order.index(c["original"]))  # stable: failures stay in order
    json.dump({"originals": originals, "cases": cases}, open(os.path.join(out, "manifest.json"), "w"), indent=1)
    print(f"{len(cases)} damaged files in {out}")


if __name__ == "__main__":
    main()
